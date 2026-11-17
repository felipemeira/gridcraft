//! Verto: real-time co-editing. Two or more people edit the same workbook and each sees the
//! others' changes as they happen, as in Google Sheets.
//!
//! A CRDT (Loro) mirrors what people entered in the workbook; it is never what the grid draws
//! from. After each top-level command [`Collab::capture`] compares the workbook with the one the
//! mirror last matched and writes the difference into the mirror, which hands out a small binary
//! update for the host to pass on. An update from someone else goes through
//! [`Collab::receive`], which turns what changed in the mirror into line inserts and deletes and
//! cell changes, applied to the workbook as one edit (`Session::collab_receive`).
//!
//! What is mirrored (prototype, stage 0):
//! - per sheet, the order of its rows and of its columns, as movable lists of ids that never
//!   change, so a line inserted by one person and a cell typed by another land where each meant;
//! - each cell's entry (constant, formula or Python code) and its format, under the ids of its
//!   row and column. Values that formulas compute are not mirrored: each copy recalculates.
//!
//! Lines past the mirrored ones (a sheet keeps ids for its used rows and columns plus a margin)
//! are named by their distance from the end of the list (`@3`), which inserts and deletes inside
//! the list move just as they move the cells. Not mirrored yet: sheets added, deleted or renamed
//! (sheets are matched by position), merges, row heights and column widths, names, comments,
//! charts. Undo and Redo are off while co-editing: the history steps of this workbook would put
//! back the others' changes too.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, Value};
use gridcraft_formula::adjust::{Axis, Edit};
use gridcraft_model::{Cell, CellStore, Formula, PyCode, Style, StyleId, StyleTable, Workbook};
use loro::event::{Diff, ListDiffItem};
use loro::{ContainerID, ExportMode, LoroDoc, LoroList, LoroMap, LoroMovableList, LoroValue, ValueOrContainer, VersionVector};
use serde_json::Value as Json;

use crate::cmd::Ctx;
use crate::{EngineError, Result, Session};

/// Rows a sheet mirrors past its last used row, and at least this many rows in all.
const ROW_MARGIN: u32 = 500;
const MIN_ROWS: u32 = 1000;
/// Columns a sheet mirrors past its last used column, and at least this many in all.
const COL_MARGIN: u32 = 20;
const MIN_COLS: u32 = 52;

fn err(e: impl std::fmt::Display) -> EngineError {
    EngineError::Other(format!("co-editing: {e}"))
}

/// A sheet's line ids, in order, and where each is.
#[derive(Clone, Debug, Default)]
struct Lines {
    ids: Vec<String>,
    index: HashMap<String, u32>,
}

impl Lines {
    fn new(ids: Vec<String>) -> Lines {
        let mut lines = Lines { ids, index: HashMap::new() };
        lines.reindex();
        lines
    }
    fn reindex(&mut self) {
        self.index = self.ids.iter().enumerate().map(|(i, id)| (id.clone(), i as u32)).collect();
    }
    /// The token naming line `i`: its id, or its distance past the last mirrored line.
    fn token(&self, i: u32) -> String {
        match self.ids.get(i as usize) {
            Some(id) => id.clone(),
            None => format!("@{}", i as usize - self.ids.len()),
        }
    }
    /// Where the line a token names is now, or `None` when its line was deleted.
    fn find(&self, token: &str) -> Option<u32> {
        match token.strip_prefix('@') {
            Some(k) => k.parse::<usize>().ok().map(|k| self.ids.len() + k).filter(|i| *i < u32::MAX as usize).map(|i| i as u32),
            None => self.index.get(token).copied(),
        }
    }
}

#[derive(Clone, Debug)]
struct SheetMirror {
    key: String,
    rows: Lines,
    cols: Lines,
}

impl SheetMirror {
    fn lines_mut(&mut self, axis: Axis) -> &mut Lines {
        match axis {
            Axis::Rows => &mut self.rows,
            Axis::Cols => &mut self.cols,
        }
    }
    fn cell_key(&self, c: CellRef) -> String {
        format!("{}|{}", self.rows.token(c.row), self.cols.token(c.col))
    }
    fn parse_key(&self, key: &str) -> Option<CellRef> {
        let (r, c) = key.split_once('|')?;
        Some(CellRef::new(self.rows.find(r)?, self.cols.find(c)?))
    }
}

fn lines_name(key: &str, axis: Axis) -> String {
    match axis {
        Axis::Rows => format!("{key}:rows"),
        Axis::Cols => format!("{key}:cols"),
    }
}

fn cells_name(key: &str) -> String {
    format!("{key}:cells")
}

const STYLE_SUFFIX: &str = "|s";

fn as_string(v: &ValueOrContainer) -> Option<String> {
    match v {
        ValueOrContainer::Value(LoroValue::String(s)) => Some(s.as_str().to_owned()),
        _ => None,
    }
}

fn get_str(map: &LoroMap, key: &str) -> Option<String> {
    map.get(key).as_ref().and_then(as_string)
}

/// Writes `value` under `key` unless it is already there; `None` removes the key.
fn put(map: &LoroMap, key: &str, value: Option<String>) -> Result<()> {
    let current = get_str(map, key);
    if current == value {
        return Ok(());
    }
    match value {
        Some(v) => map.insert(key, v).map_err(err),
        None => map.delete(key).map_err(err),
    }
}

fn movable_strings(list: &LoroMovableList) -> Vec<String> {
    list.to_vec().into_iter().filter_map(|v| if let LoroValue::String(s) = v { Some(s.as_str().to_owned()) } else { None }).collect()
}

fn list_strings(list: &LoroList) -> Vec<String> {
    list.to_vec().into_iter().filter_map(|v| if let LoroValue::String(s) = v { Some(s.as_str().to_owned()) } else { None }).collect()
}

/// What a person entered in a cell, as the mirror keeps it: the formula, the Python code or the
/// constant. `None` for a cell with nothing entered (format only, or empty).
fn entry(cell: &Cell) -> Option<String> {
    let json = if let Some(f) = &cell.formula {
        serde_json::json!({ "f": &**f })
    } else if let Some(py) = &cell.py {
        serde_json::json!({ "py": &**py })
    } else if cell.value != Value::Empty {
        serde_json::json!({ "v": &cell.value })
    } else {
        return None;
    };
    serde_json::to_string(&json).ok()
}

/// A cell as the mirror holds it: its entry and its format (as the style's JSON).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellSpec {
    pub entry: Option<String>,
    pub style: Option<String>,
}

impl CellSpec {
    /// The cell to store, or `None` when there is nothing to store.
    fn to_cell(&self, styles: &mut StyleTable) -> Option<Cell> {
        let json: Option<Json> = self.entry.as_deref().and_then(|s| serde_json::from_str(s).ok());
        let mut cell = match json {
            Some(j) if j.get("f").is_some() => {
                let f: Formula = serde_json::from_value(j["f"].clone()).ok()?;
                let expr = gridcraft_formula::parse(&f.text).ok().map(Arc::new);
                Cell { value: Value::Empty, formula: Some(Arc::new(Formula { expr, ..f })), style: StyleId::DEFAULT, py: None }
            }
            Some(j) if j.get("py").is_some() => {
                let py: PyCode = serde_json::from_value(j["py"].clone()).ok()?;
                Cell { value: Value::Empty, formula: None, style: StyleId::DEFAULT, py: Some(Arc::new(py)) }
            }
            Some(j) if j.get("v").is_some() => Cell::value(serde_json::from_value(j["v"].clone()).unwrap_or(Value::Empty)),
            _ => Cell::value(Value::Empty),
        };
        if let Some(style) = self.style.as_deref().and_then(|s| serde_json::from_str::<Style>(s).ok()) {
            cell.style = styles.intern(style);
        }
        let empty = cell.formula.is_none() && cell.py.is_none() && cell.value == Value::Empty && cell.style == StyleId::DEFAULT;
        (!empty).then_some(cell)
    }
}

/// A line insert or delete someone else made.
#[derive(Clone, Debug, PartialEq)]
pub struct LineChange {
    pub sheet: usize,
    pub axis: Axis,
    pub insert: bool,
    pub at: u32,
    pub count: u32,
}

/// What an update from someone else changed, in the workbook's terms.
#[derive(Clone, Debug, Default)]
pub struct Remote {
    /// Line inserts and deletes, in the order they apply.
    pub lines: Vec<LineChange>,
    /// Cells to set (or clear), where they are after `lines`.
    pub cells: Vec<(usize, CellRef, CellSpec)>,
    /// Sheets whose lines no longer matched the mirror: all their cells, rebuilt from it.
    pub rebuilt: Vec<(usize, Vec<(CellRef, CellSpec)>)>,
}

impl Remote {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.cells.is_empty() && self.rebuilt.is_empty()
    }

    /// Applies the changes inside an edit.
    pub(crate) fn apply(self, cx: &mut Ctx) -> Result<()> {
        cx.protection_checked = true;
        for l in &self.lines {
            crate::cmd::sheet::apply_line_edit(cx, l.sheet, l.axis, l.insert, l.at, l.count)?;
        }
        for (si, cells) in self.rebuilt {
            let mut store = CellStore::new();
            for (c, spec) in cells {
                if let Some(cell) = spec.to_cell(&mut cx.wb.styles) {
                    store.set(c, cell);
                }
            }
            cx.sheet_mut(si)?.cells = store;
            cx.structural = true;
        }
        for (si, c, spec) in self.cells {
            let cell = spec.to_cell(&mut cx.wb.styles);
            let sh = cx.sheet_mut(si)?;
            match cell {
                Some(cell) => sh.cells.set(c, cell),
                None => {
                    sh.cells.remove(c);
                }
            }
            cx.touch(si, c);
        }
        Ok(())
    }
}

/// The mirror of one open workbook.
#[derive(Clone)]
pub struct Collab {
    doc: LoroDoc,
    sheets: Vec<SheetMirror>,
    /// The workbook the mirror matches.
    base: Arc<Workbook>,
    /// Line inserts and deletes made since `base` (the sheet's name, the edit), in order.
    pending: Vec<(String, Edit)>,
    /// Updates for the others, oldest first.
    outbox: Vec<Vec<u8>>,
    /// What the others have been sent (or sent us).
    sent: VersionVector,
    next_id: u64,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").field("peer", &self.doc.peer_id()).field("sheets", &self.sheets.len()).field("outbox", &self.outbox.len()).finish()
    }
}

impl Collab {
    /// Starts a shared document from `wb`: the first person to open the file.
    pub fn seed(wb: Arc<Workbook>) -> Result<Collab> {
        let doc = LoroDoc::new();
        let list = doc.get_list("sheets");
        let mut c = Collab { doc: doc.clone(), sheets: Vec::new(), base: wb.clone(), pending: Vec::new(), outbox: Vec::new(), sent: VersionVector::default(), next_id: 0 };
        for (i, sh) in wb.sheets.iter().enumerate() {
            let key = format!("s{i}");
            list.push(key.as_str()).map_err(err)?;
            let used = sh.cells.used_range();
            let nrows = used.map_or(0, |u| u.end.row + 1).saturating_add(ROW_MARGIN).max(MIN_ROWS).min(MAX_ROWS);
            let ncols = used.map_or(0, |u| u.end.col + 1).saturating_add(COL_MARGIN).max(MIN_COLS).min(MAX_COLS);
            let row_ids: Vec<String> = (0..nrows).map(|r| format!("r{r}")).collect();
            let col_ids: Vec<String> = (0..ncols).map(|k| format!("c{k}")).collect();
            for (axis, ids) in [(Axis::Rows, &row_ids), (Axis::Cols, &col_ids)] {
                let l = doc.get_movable_list(lines_name(&key, axis).as_str());
                for id in ids {
                    l.push(id.as_str()).map_err(err)?;
                }
            }
            c.sheets.push(SheetMirror { key, rows: Lines::new(row_ids), cols: Lines::new(col_ids) });
            for (cref, cell) in sh.cells.iter() {
                c.write_cell(i, cref, Some(cell), &wb)?;
            }
        }
        doc.commit();
        c.sent = doc.oplog_vv();
        Ok(c)
    }

    /// Joins a shared document from its snapshot: `wb` is the same file opened here, and the
    /// workbook returned has the shared cells in place of its own (not yet recalculated).
    pub fn join(snapshot: &[u8], wb: &Workbook) -> Result<(Collab, Workbook)> {
        let doc = LoroDoc::new();
        doc.import(snapshot).map_err(err)?;
        let keys = list_strings(&doc.get_list("sheets"));
        if keys.len() != wb.sheets.len() {
            return Err(EngineError::Other(format!(
                "co-editing: the shared workbook has {} sheets and this one {}; open the same file",
                keys.len(),
                wb.sheets.len()
            )));
        }
        let mut c = Collab { doc: doc.clone(), sheets: Vec::new(), base: Arc::new(wb.clone()), pending: Vec::new(), outbox: Vec::new(), sent: doc.oplog_vv(), next_id: 0 };
        for key in keys {
            let rows = Lines::new(movable_strings(&doc.get_movable_list(lines_name(&key, Axis::Rows).as_str())));
            let cols = Lines::new(movable_strings(&doc.get_movable_list(lines_name(&key, Axis::Cols).as_str())));
            c.sheets.push(SheetMirror { key, rows, cols });
        }
        let mut wb = wb.clone();
        for si in 0..c.sheets.len() {
            let mut store = CellStore::new();
            for (cref, spec) in c.all_cells(si) {
                if let Some(cell) = spec.to_cell(&mut wb.styles) {
                    store.set(cref, cell);
                }
            }
            if let Some(sh) = wb.sheets.get_mut(si) {
                Arc::make_mut(sh).cells = store;
            }
        }
        Ok((c, wb))
    }

    /// The whole shared document, for someone joining.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        self.doc.export(ExportMode::Snapshot).map_err(err)
    }

    /// The updates for the others made since the last call, oldest first.
    pub fn take_outbox(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox)
    }

    /// Notes line inserts and deletes an edit made (the sheet's name, the edit).
    pub(crate) fn note_edits(&mut self, edits: &[(String, Edit)]) {
        self.pending.extend(edits.iter().cloned());
    }

    /// The mirror now matches `wb` (after someone else's changes were applied to it).
    pub(crate) fn rebase(&mut self, wb: &Arc<Workbook>) {
        self.base = wb.clone();
        self.pending.clear();
    }

    /// Writes what changed between the workbook the mirror matched and `after` into the mirror,
    /// and queues the update for the others.
    pub fn capture(&mut self, after: &Arc<Workbook>) -> Result<()> {
        if Arc::ptr_eq(&self.base, after) {
            self.pending.clear();
            return Ok(());
        }
        let edits = std::mem::take(&mut self.pending);
        let mut reshaped: HashSet<usize> = HashSet::new();
        for (name, e) in &edits {
            let Some(si) = after.sheet_index(name).or_else(|| self.base.sheet_index(name)) else { continue };
            if si >= self.sheets.len() {
                continue;
            }
            match *e {
                Edit::Insert { axis, at, count } => self.insert_lines(si, axis, at, count)?,
                Edit::Delete { axis, at, count } => self.delete_lines(si, axis, at, count)?,
                _ => continue,
            }
            reshaped.insert(si);
        }
        let base = self.base.clone();
        for si in 0..after.sheets.len().min(self.sheets.len()) {
            let a = &after.sheets[si];
            let b = base.sheets.get(si);
            if b.is_some_and(|b| Arc::ptr_eq(b, a)) {
                continue;
            }
            match b.filter(|_| !reshaped.contains(&si)) {
                Some(b) => {
                    for c in b.cells.diff(&a.cells) {
                        self.write_cell(si, c, a.cells.get(c), after)?;
                    }
                }
                None => self.sync_sheet(si, after)?,
            }
        }
        self.base = after.clone();
        self.flush()
    }

    fn flush(&mut self) -> Result<()> {
        self.doc.commit();
        let now = self.doc.oplog_vv();
        if now != self.sent {
            let bytes = self.doc.export(ExportMode::updates(&self.sent)).map_err(err)?;
            self.outbox.push(bytes);
            self.sent = now;
        }
        Ok(())
    }

    fn new_ids(&mut self, count: u32) -> Vec<String> {
        let peer = self.doc.peer_id();
        (0..count)
            .map(|_| {
                self.next_id += 1;
                format!("{peer:x}.{}", self.next_id)
            })
            .collect()
    }

    fn insert_lines(&mut self, si: usize, axis: Axis, at: u32, count: u32) -> Result<()> {
        let at = at as usize;
        let len = match axis {
            Axis::Rows => self.sheets[si].rows.ids.len(),
            Axis::Cols => self.sheets[si].cols.ids.len(),
        };
        // Past the mirrored lines the cells keep their distance from the end of the list, which
        // an insert there would not move: those lines stay unmirrored.
        if at > len {
            log::warn!("co-editing: lines inserted past the shared ones ({at} > {len}) are not shared");
            return Ok(());
        }
        let ids = self.new_ids(count);
        let list = self.doc.get_movable_list(lines_name(&self.sheets[si].key, axis).as_str());
        for (k, id) in ids.iter().enumerate() {
            list.insert(at + k, id.as_str()).map_err(err)?;
        }
        let lines = self.sheets[si].lines_mut(axis);
        lines.ids.splice(at..at, ids);
        lines.reindex();
        Ok(())
    }

    fn delete_lines(&mut self, si: usize, axis: Axis, at: u32, count: u32) -> Result<()> {
        let at = at as usize;
        let len = match axis {
            Axis::Rows => self.sheets[si].rows.ids.len(),
            Axis::Cols => self.sheets[si].cols.ids.len(),
        };
        if at >= len {
            log::warn!("co-editing: lines deleted past the shared ones ({at} >= {len}) are not shared");
            return Ok(());
        }
        let n = (count as usize).min(len - at);
        self.doc.get_movable_list(lines_name(&self.sheets[si].key, axis).as_str()).delete(at, n).map_err(err)?;
        let lines = self.sheets[si].lines_mut(axis);
        lines.ids.drain(at..at + n);
        lines.reindex();
        Ok(())
    }

    /// The key of a style in the mirror's style table, adding the style when it is new there.
    fn style_key(&self, wb: &Workbook, id: StyleId) -> Result<Option<String>> {
        if id == StyleId::DEFAULT {
            return Ok(None);
        }
        let json = serde_json::to_string(wb.styles.get(id)).map_err(err)?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        json.hash(&mut h);
        let key = format!("{:016x}", h.finish());
        let styles = self.doc.get_map("styles");
        if get_str(&styles, &key).is_none() {
            styles.insert(&key, json).map_err(err)?;
        }
        Ok(Some(key))
    }

    fn write_cell(&self, si: usize, c: CellRef, cell: Option<&Cell>, wb: &Workbook) -> Result<()> {
        let m = &self.sheets[si];
        let key = m.cell_key(c);
        let map = self.doc.get_map(cells_name(&m.key).as_str());
        put(&map, &key, cell.and_then(entry))?;
        let style = match cell {
            Some(cell) => self.style_key(wb, cell.style)?,
            None => None,
        };
        put(&map, &format!("{key}{STYLE_SUFFIX}"), style)
    }

    /// Writes every cell of sheet `si` into the mirror and drops the entries of cells that are
    /// gone (or whose line was deleted): after a line insert or delete.
    fn sync_sheet(&self, si: usize, wb: &Workbook) -> Result<()> {
        let Some(sh) = wb.sheets.get(si) else { return Ok(()) };
        for (c, cell) in sh.cells.iter() {
            self.write_cell(si, c, Some(cell), wb)?;
        }
        let m = &self.sheets[si];
        let map = self.doc.get_map(cells_name(&m.key).as_str());
        let stale: Vec<String> = map
            .keys()
            .map(|k| k.to_string())
            .filter(|k| {
                let base = k.strip_suffix(STYLE_SUFFIX).unwrap_or(k.as_str());
                m.parse_key(base).is_none_or(|c| sh.cells.get(c).is_none())
            })
            .collect();
        for k in stale {
            map.delete(&k).map_err(err)?;
        }
        Ok(())
    }

    fn read_cell(&self, si: usize, key: &str) -> CellSpec {
        let map = self.doc.get_map(cells_name(&self.sheets[si].key).as_str());
        let styles = self.doc.get_map("styles");
        CellSpec { entry: get_str(&map, key), style: get_str(&map, &format!("{key}{STYLE_SUFFIX}")).and_then(|h| get_str(&styles, &h)) }
    }

    /// Every cell of sheet `si` the mirror holds, where it is now.
    fn all_cells(&self, si: usize) -> Vec<(CellRef, CellSpec)> {
        let m = &self.sheets[si];
        let map = self.doc.get_map(cells_name(&m.key).as_str());
        let keys: BTreeSet<String> = map
            .keys()
            .map(|k| {
                let k: &str = &k;
                k.strip_suffix(STYLE_SUFFIX).unwrap_or(k).to_string()
            })
            .collect();
        keys.into_iter().filter_map(|k| Some((m.parse_key(&k)?, self.read_cell(si, &k)))).collect()
    }

    fn sheet_of(&self, key: &str) -> Option<usize> {
        self.sheets.iter().position(|m| m.key == key)
    }

    /// Takes in an update from someone else and says what it changed in the workbook.
    pub fn receive(&mut self, update: &[u8]) -> Result<Remote> {
        let before = self.doc.state_frontiers();
        self.doc.import(update).map_err(err)?;
        let after = self.doc.state_frontiers();
        // Everything local was sent when it was made; what came in needs no sending back.
        self.sent = self.doc.oplog_vv();
        let mut remote = Remote::default();
        if before == after {
            return Ok(remote);
        }
        let batch = self.doc.diff(&before, &after).map_err(err)?;
        let mut cell_keys: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let mut moved: BTreeSet<usize> = BTreeSet::new();
        for (cid, diff) in batch.iter() {
            let ContainerID::Root { name, .. } = cid else { continue };
            let name: &str = name.as_ref();
            let Some((key, kind)) = name.rsplit_once(':') else { continue };
            let Some(si) = self.sheet_of(key) else { continue };
            match (kind, diff) {
                ("rows" | "cols", Diff::List(items)) => {
                    let axis = if kind == "rows" { Axis::Rows } else { Axis::Cols };
                    self.apply_list(si, axis, items, &mut remote);
                    moved.insert(si);
                }
                ("cells", Diff::Map(delta)) => {
                    let keys = cell_keys.entry(si).or_default();
                    for k in delta.updated.keys() {
                        let k: &str = k.as_ref();
                        keys.insert(k.strip_suffix(STYLE_SUFFIX).unwrap_or(k).to_string());
                    }
                }
                _ => {}
            }
        }
        // The lists as the diff left them must be the lists in the mirror; if not, the sheet is
        // rebuilt from the mirror rather than left wrong.
        for si in moved {
            let key = self.sheets[si].key.clone();
            let mut rebuild = false;
            for axis in [Axis::Rows, Axis::Cols] {
                let ids = movable_strings(&self.doc.get_movable_list(lines_name(&key, axis).as_str()));
                let lines = self.sheets[si].lines_mut(axis);
                if lines.ids != ids {
                    log::warn!("co-editing: sheet {key} lines out of step; rebuilding it");
                    *lines = Lines::new(ids);
                    rebuild = true;
                }
            }
            if rebuild {
                remote.lines.retain(|l| l.sheet != si);
                cell_keys.remove(&si);
                remote.rebuilt.push((si, self.all_cells(si)));
            }
        }
        for (si, keys) in cell_keys {
            for k in keys {
                if let Some(c) = self.sheets[si].parse_key(&k) {
                    let spec = self.read_cell(si, &k);
                    remote.cells.push((si, c, spec));
                }
            }
        }
        Ok(remote)
    }

    /// Follows a list diff on the sheet's lines, noting the inserts and deletes it makes.
    fn apply_list(&mut self, si: usize, axis: Axis, items: &[ListDiffItem], remote: &mut Remote) {
        let lines = self.sheets[si].lines_mut(axis);
        let mut i = 0usize;
        for item in items {
            match item {
                ListDiffItem::Retain { retain } => i += retain,
                ListDiffItem::Delete { delete } => {
                    let end = (i + delete).min(lines.ids.len());
                    let n = end.saturating_sub(i);
                    lines.ids.drain(i..end);
                    if n > 0 {
                        remote.lines.push(LineChange { sheet: si, axis, insert: false, at: i as u32, count: n as u32 });
                    }
                }
                ListDiffItem::Insert { insert, .. } => {
                    let ids: Vec<String> = insert.iter().filter_map(as_string).collect();
                    let n = ids.len();
                    let at = i.min(lines.ids.len());
                    lines.ids.splice(at..at, ids);
                    if n > 0 {
                        remote.lines.push(LineChange { sheet: si, axis, insert: true, at: at as u32, count: n as u32 });
                    }
                    i = at + n;
                }
            }
        }
        lines.reindex();
    }
}

impl Session {
    /// Whether the active workbook is being edited with others.
    pub fn collab_active(&self) -> bool {
        self.active().is_some_and(|d| d.collab.is_some())
    }

    /// Shares the active workbook (the first person to open it) and returns the snapshot that
    /// others join from.
    pub fn collab_seed(&mut self) -> Result<Vec<u8>> {
        let d = self.doc_mut()?;
        let c = Collab::seed(d.wb.clone())?;
        let snapshot = c.snapshot()?;
        d.collab = Some(Box::new(c));
        Ok(snapshot)
    }

    /// Joins a shared workbook from its snapshot. The active workbook must be the same file; its
    /// cells are replaced by the shared ones.
    pub fn collab_join(&mut self, snapshot: &[u8]) -> Result<()> {
        let d = self.doc_mut()?;
        let (mut c, mut wb) = Collab::join(snapshot, &d.wb)?;
        let mut calc = gridcraft_calc::Calc::new();
        calc.recalc_all(&mut wb);
        let wb = Arc::new(wb);
        c.rebase(&wb);
        d.calc = calc;
        d.wb = wb.clone();
        d.saved = wb;
        d.undo.clear();
        d.redo.clear();
        d.revision += 1;
        d.collab = Some(Box::new(c));
        Ok(())
    }

    /// Applies an update from someone else to the active workbook.
    pub fn collab_receive(&mut self, update: &[u8]) -> Result<()> {
        let d = self.doc_mut()?;
        let Some(c) = d.collab.as_mut() else { return Err(EngineError::Other("co-editing: this workbook is not shared".into())) };
        let remote = c.receive(update)?;
        if remote.is_empty() {
            return Ok(());
        }
        let applied = crate::cmd::commit(d, |cx| remote.apply(cx));
        // Whatever the edit did, the mirror now stands for the workbook as it is.
        if let Some(c) = d.collab.as_mut() {
            c.rebase(&d.wb);
        }
        d.revision += 1;
        applied
    }

    /// The snapshot of the active workbook's shared document, for someone joining.
    pub fn collab_snapshot(&self) -> Result<Vec<u8>> {
        self.doc()?.collab.as_ref().ok_or_else(|| EngineError::Other("co-editing: this workbook is not shared".into()))?.snapshot()
    }

    /// The updates for the others, from every shared workbook.
    pub fn collab_outbox(&mut self) -> Vec<Vec<u8>> {
        self.docs.iter_mut().filter_map(|d| d.collab.as_mut()).flat_map(|c| c.take_outbox()).collect()
    }

    /// Stops sharing the active workbook.
    pub fn collab_stop(&mut self) {
        if let Some(d) = self.active_mut() {
            d.collab = None;
        }
    }

    /// After a top-level command: what it changed goes into the mirror.
    pub(crate) fn collab_capture(&mut self, uid: u64) {
        if let Some(d) = self.docs.iter_mut().find(|d| d.uid == uid)
            && let Some(c) = d.collab.as_mut()
            && let Err(e) = c.capture(&d.wb)
        {
            log::warn!("{e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn session() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        s
    }

    fn set(s: &mut Session, cell: &str, input: &str) {
        s.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
    }

    fn shown(s: &Session, cell: &str) -> Value {
        let c = CellRef::parse(cell).unwrap();
        s.doc().unwrap().wb.sheets[0].cells.get(c).map(|c| c.value.clone()).unwrap_or(Value::Empty)
    }

    fn formula(s: &Session, cell: &str) -> Option<String> {
        let c = CellRef::parse(cell).unwrap();
        s.doc().unwrap().wb.sheets[0].cells.get(c).and_then(|c| c.formula.as_ref().map(|f| f.text.clone()))
    }

    /// Delivers what `from` has for the others to `to`.
    fn pass(from: &mut Session, to: &mut Session) {
        for u in from.collab_outbox() {
            to.collab_receive(&u).unwrap();
        }
    }

    fn pair() -> (Session, Session) {
        let mut a = session();
        set(&mut a, "A1", "5");
        set(&mut a, "A2", "=A1*2");
        let snapshot = a.collab_seed().unwrap();
        let mut b = session();
        b.collab_join(&snapshot).unwrap();
        (a, b)
    }

    #[test]
    fn joining_takes_the_shared_cells() {
        let (_, b) = pair();
        assert_eq!(shown(&b, "A1"), Value::Number(5.0));
        assert_eq!(formula(&b, "A2").as_deref(), Some("A1*2"));
        assert_eq!(shown(&b, "A2"), Value::Number(10.0));
    }

    #[test]
    fn a_typed_cell_reaches_the_other() {
        let (mut a, mut b) = pair();
        set(&mut a, "A1", "7");
        set(&mut a, "C3", "texto");
        pass(&mut a, &mut b);
        assert_eq!(shown(&b, "A1"), Value::Number(7.0));
        assert_eq!(shown(&b, "A2"), Value::Number(14.0), "the other recalculates");
        assert_eq!(shown(&b, "C3"), Value::Text("texto".into()));
        // Nothing echoes back.
        assert!(b.collab_outbox().is_empty());
    }

    #[test]
    fn a_cleared_cell_and_a_format_reach_the_other() {
        let (mut a, mut b) = pair();
        a.execute("home.bold", json!({"range": "A1"})).unwrap();
        a.execute("edit.clearContents", json!({"range": "A2"})).unwrap();
        pass(&mut a, &mut b);
        let wb = &b.doc().unwrap().wb;
        let cell = wb.sheets[0].cells.get(CellRef::parse("A1").unwrap()).unwrap();
        assert!(wb.styles.get(cell.style).font.bold);
        assert_eq!(formula(&b, "A2"), None);
    }

    #[test]
    fn rows_inserted_by_one_and_cells_typed_by_the_other_both_land_right() {
        let (mut a, mut b) = pair();
        // A inserts a row above row 1 while B types in A1 (the 5 that A will see in A2).
        a.execute("home.insertRows", json!({"rows": "1:1"})).unwrap();
        set(&mut b, "A1", "9");
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        for s in [&a, &b] {
            assert_eq!(shown(s, "A1"), Value::Empty);
            assert_eq!(shown(s, "A2"), Value::Number(9.0));
            assert_eq!(formula(s, "A3").as_deref(), Some("A2*2"));
            assert_eq!(shown(s, "A3"), Value::Number(18.0));
        }
    }

    #[test]
    fn deleted_rows_go_for_both() {
        let (mut a, mut b) = pair();
        set(&mut a, "A5", "x");
        a.execute("home.deleteRows", json!({"rows": "1:1"})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(formula(&b, "A1"), formula(&a, "A1"));
        assert_eq!(shown(&b, "A1"), shown(&a, "A1"));
        assert_eq!(shown(&b, "A4"), Value::Text("x".into()));
    }

    #[test]
    fn the_same_cell_typed_by_both_ends_the_same_for_both() {
        let (mut a, mut b) = pair();
        set(&mut a, "B1", "de A");
        set(&mut b, "B1", "de B");
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        assert_eq!(shown(&a, "B1"), shown(&b, "B1"));
    }

    #[test]
    fn undo_is_off_while_shared() {
        let (mut a, _) = pair();
        set(&mut a, "A1", "1");
        assert!(a.execute("edit.undo", json!({})).is_err());
    }
}
