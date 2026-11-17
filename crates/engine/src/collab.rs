//! Verto: real-time co-editing. Two or more people edit the same workbook and each sees the
//! others' changes as they happen, as in Google Sheets.
//!
//! A CRDT (Loro) mirrors what people entered in the workbook; it is never what the grid draws
//! from. After each top-level command [`Collab::capture`] compares the workbook with the one the
//! mirror last matched and writes the difference into the mirror, which hands out a small binary
//! update for the host to pass on. An update from someone else goes through
//! [`Collab::receive`], which turns what changed in the mirror into changes of the workbook
//! (`Session::collab_receive`).
//!
//! What is mirrored:
//! - the sheets, in order, each under a key that never changes (`sheet-order`), with its name;
//!   sheets added, renamed, moved and deleted are made again here with the sheet commands, so
//!   formulas follow them as they do for the person who made the change;
//! - per sheet, the order of its rows and of its columns, as movable lists of ids that never
//!   change, so a line inserted by one person and a cell typed by another land where each meant;
//! - each cell's entry (constant, formula or Python code) and its format, under the ids of its
//!   row and column. Values that formulas compute are not mirrored: each copy recalculates;
//! - row heights and column widths, merged areas, notes and comments, and hyperlinks, also under
//!   the line ids;
//! - the sheet's other settings (conditional formats, validation, tables, filters, charts,
//!   pictures, shapes, frozen panes, tab colour, protection, page setup…), each as a whole: when
//!   two people change the same one at once, the last change wins.
//!
//! Lines past the mirrored ones (a sheet keeps ids for its used rows and columns plus a margin)
//! are named by their distance from the end of the list (`@3`), which inserts and deletes inside
//! the list move just as they move the cells. Not mirrored yet: defined names and other
//! workbook-wide settings, pictures in cells. Undo and Redo of a shared workbook go through the
//! mirror: only this person's own changes, adjusted to what the others did since
//! ([`Collab::undo`]); the workbook's own history steps would put back the others' changes too.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef, Value};
use gridcraft_formula::adjust::{Axis, Edit};
use gridcraft_model::{Cell, CellStore, Comment, Formula, Hyperlink, LineInfo, PyCode, Sheet, Style, StyleId, StyleTable, Workbook};
use loro::event::{Diff, ListDiffItem};
use loro::{ContainerID, ExportMode, Frontiers, LoroDoc, LoroMap, LoroMovableList, LoroValue, UndoManager, ValueOrContainer, VersionVector};
use serde_json::Value as Json;

use crate::cmd::Ctx;
use crate::{EngineError, Result, Session};

/// Rows a sheet mirrors past its last used row, and at least this many rows in all.
const ROW_MARGIN: u32 = 500;
const MIN_ROWS: u32 = 1000;
/// Columns a sheet mirrors past its last used column, and at least this many in all.
const COL_MARGIN: u32 = 20;
const MIN_COLS: u32 = 52;

/// The sheets' keys, in order.
const SHEET_ORDER: &str = "sheet-order";

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
    /// The sheet's [`gridcraft_model::SheetUid`] here (0 until the sheet exists here).
    uid: u64,
    rows: Lines,
    cols: Lines,
}

impl SheetMirror {
    fn lines(&self, axis: Axis) -> &Lines {
        match axis {
            Axis::Rows => &self.rows,
            Axis::Cols => &self.cols,
        }
    }
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
    fn range_key(&self, r: RangeRef) -> String {
        format!("{}|{}", self.cell_key(r.start), self.cell_key(r.end))
    }
    fn parse_range(&self, key: &str) -> Option<RangeRef> {
        let parts: Vec<&str> = key.split('|').collect();
        let [r0, c0, r1, c1] = parts.as_slice() else { return None };
        let start = CellRef::new(self.rows.find(r0)?, self.cols.find(c0)?);
        let end = CellRef::new(self.rows.find(r1)?, self.cols.find(c1)?);
        Some(RangeRef::new(start, end))
    }
}

/// The mirror's containers of a sheet.
fn part(key: &str, kind: &str) -> String {
    format!("{key}:{kind}")
}

fn lines_name(key: &str, axis: Axis) -> String {
    part(key, if axis == Axis::Rows { "rows" } else { "cols" })
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

fn map_keys(map: &LoroMap) -> Vec<String> {
    map.keys().map(|k| k.to_string()).collect()
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

/// The sheet's other settings, each mirrored as a whole (the last change wins): what is compared
/// and written after a command, and set from the mirror when someone else changed it.
macro_rules! sheet_settings {
    ($($f:ident),* $(,)?) => {
        /// The settings of `a` that differ from `b` (all of them without `b`), as JSON.
        fn changed_settings(b: Option<&Sheet>, a: &Sheet) -> Vec<(&'static str, String)> {
            let mut out = Vec::new();
            $(
                if b.is_none_or(|b| b.$f != a.$f) {
                    match serde_json::to_string(&a.$f) {
                        Ok(s) => out.push((stringify!($f), s)),
                        Err(e) => log::warn!("co-editing: {}: {e}", stringify!($f)),
                    }
                }
            )*
            out
        }
        fn apply_setting(sh: &mut Sheet, key: &str, json: &str) {
            match key {
                $(
                    stringify!($f) => match serde_json::from_str(json) {
                        Ok(v) => sh.$f = v,
                        Err(e) => log::warn!("co-editing: {key}: {e}"),
                    },
                )*
                _ => {}
            }
        }
    };
}

sheet_settings!(
    freeze,
    tab_color,
    visibility,
    show_gridlines,
    show_headings,
    show_formulas,
    show_zeros,
    show_outline_symbols,
    summary_below,
    summary_right,
    right_to_left,
    default_col_width,
    default_row_height,
    cond_formats,
    validations,
    tables,
    autofilter,
    charts,
    images,
    shapes,
    sparklines,
    slicers,
    controls,
    pivots,
    protection,
    edit_ranges,
    print,
);

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

/// A part of a sheet someone else changed, rebuilt whole from the mirror.
#[derive(Clone, Debug)]
pub enum Feature {
    Rows(BTreeMap<u32, LineInfo>),
    Cols(BTreeMap<u32, LineInfo>),
    Merges(Vec<RangeRef>),
    Comments(BTreeMap<CellRef, Comment>),
    Links(BTreeMap<CellRef, Hyperlink>),
    /// A setting (see `sheet_settings!`) and its JSON.
    Setting(String, String),
}

/// The sheets as someone else left them: the order of their keys, with their names and where
/// each is here (`uid` 0 for a sheet to add), and the sheets here to delete.
#[derive(Clone, Debug, Default)]
pub struct SheetPlan {
    pub deleted: Vec<u64>,
    pub order: Vec<(String, u64, String)>,
}

/// What an update from someone else changed, in the workbook's terms.
#[derive(Clone, Debug, Default)]
pub struct Remote {
    /// Sheets added, moved or deleted (applied first, with the sheet commands).
    pub sheets: Option<SheetPlan>,
    /// Sheets renamed (where they are after `sheets`, the new name).
    pub renames: Vec<(usize, String)>,
    /// Line inserts and deletes, in the order they apply.
    pub lines: Vec<LineChange>,
    /// Cells to set (or clear), where they are after `lines`.
    pub cells: Vec<(usize, CellRef, CellSpec)>,
    /// Sheets rebuilt from the mirror (new here, or whose lines no longer matched it): all their
    /// cells.
    pub rebuilt: Vec<(usize, Vec<(CellRef, CellSpec)>)>,
    /// Parts of sheets to set from the mirror.
    pub features: Vec<(usize, Feature)>,
}

impl Remote {
    pub fn is_empty(&self) -> bool {
        self.sheets.is_none()
            && self.renames.is_empty()
            && self.lines.is_empty()
            && self.cells.is_empty()
            && self.rebuilt.is_empty()
            && self.features.is_empty()
    }

    /// Applies the changes to cells, lines and sheet parts inside an edit (the sheets and their
    /// names are done before, with the sheet commands).
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
        for (si, feature) in self.features {
            let sh = cx.sheet_mut(si)?;
            match feature {
                Feature::Rows(rows) => sh.rows = rows,
                Feature::Cols(cols) => sh.cols = cols,
                Feature::Merges(merges) => sh.merges = merges,
                Feature::Comments(comments) => sh.comments = comments,
                Feature::Links(links) => sh.hyperlinks = links,
                Feature::Setting(key, json) => {
                    apply_setting(sh, &key, &json);
                    // Conditional formats, validation, tables and the like can change what
                    // formulas and the grid show.
                    cx.recalc_all = true;
                }
            }
        }
        Ok(())
    }
}

/// Another person editing the shared workbook: who, in what colour, and what they have selected.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub color: [u8; 3],
    /// The sheet they are on, by position.
    pub sheet: usize,
    pub ranges: Vec<RangeRef>,
    pub active: CellRef,
}

/// The mirror of one open workbook.
#[derive(Clone)]
pub struct Collab {
    doc: LoroDoc,
    /// The sheets, in the workbook's order.
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
    /// Undo and Redo of this person's own changes (each top-level command is one step).
    undo: Arc<std::sync::Mutex<UndoManager>>,
}

impl std::fmt::Debug for Collab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collab").field("peer", &self.doc.peer_id()).field("sheets", &self.sheets.len()).field("outbox", &self.outbox.len()).finish()
    }
}

impl Collab {
    fn empty(doc: LoroDoc, base: Arc<Workbook>) -> Collab {
        let undo = new_undo(&doc);
        Collab { sent: doc.oplog_vv(), doc, sheets: Vec::new(), base, pending: Vec::new(), outbox: Vec::new(), next_id: 0, undo }
    }

    /// Starts a shared document from `wb`: the first person to open the file.
    pub fn seed(wb: Arc<Workbook>) -> Result<Collab> {
        let doc = LoroDoc::new();
        let mut c = Collab::empty(doc.clone(), wb.clone());
        let order = doc.get_movable_list(SHEET_ORDER);
        for (i, sh) in wb.sheets.iter().enumerate() {
            let key = format!("s{i}");
            order.push(key.as_str()).map_err(err)?;
            let m = c.new_sheet_mirror(key, sh)?;
            c.sheets.push(m);
            c.sync_sheet(i, &wb)?;
        }
        doc.commit();
        c.sent = doc.oplog_vv();
        // The seed is where history starts: nothing before it is undone.
        c.undo = new_undo(&doc);
        Ok(c)
    }

    /// Joins a shared document from its snapshot: `wb` is the same file opened here, and the
    /// workbook returned has the shared sheets and cells in place of its own (not yet
    /// recalculated). Sheets are matched by name, then (for the file's own sheets) by position.
    pub fn join(snapshot: &[u8], wb: &Workbook) -> Result<(Collab, Workbook)> {
        let doc = LoroDoc::new();
        doc.import(snapshot).map_err(err)?;
        let mut c = Collab::empty(doc.clone(), Arc::new(wb.clone()));
        let keys = movable_strings(&doc.get_movable_list(SHEET_ORDER));
        let mut out = wb.clone();
        let mut taken: HashSet<usize> = HashSet::new();
        let mut sheets: Vec<Arc<Sheet>> = Vec::new();
        for key in &keys {
            let name = get_str(&doc.get_map(part(key, "meta").as_str()), "name").unwrap_or_else(|| key.clone());
            let by_name = wb.sheets.iter().position(|s| s.name.eq_ignore_ascii_case(&name)).filter(|i| !taken.contains(i));
            let by_place = key.strip_prefix('s').and_then(|n| n.parse::<usize>().ok()).filter(|i| *i < wb.sheets.len() && !taken.contains(i));
            let sheet = match by_name.or(by_place) {
                Some(i) => {
                    taken.insert(i);
                    let mut sh = (*wb.sheets[i]).clone();
                    sh.name = name;
                    sh
                }
                None => Sheet::new(name),
            };
            let rows = Lines::new(movable_strings(&doc.get_movable_list(lines_name(key, Axis::Rows).as_str())));
            let cols = Lines::new(movable_strings(&doc.get_movable_list(lines_name(key, Axis::Cols).as_str())));
            c.sheets.push(SheetMirror { key: key.clone(), uid: sheet.uid.get(), rows, cols });
            sheets.push(Arc::new(sheet));
        }
        if sheets.is_empty() {
            return Err(EngineError::Other("co-editing: the shared workbook has no sheets".into()));
        }
        out.sheets = sheets;
        if out.active_sheet() >= out.sheets.len() {
            out.set_active_sheet(0);
        }
        for si in 0..c.sheets.len() {
            let mut store = CellStore::new();
            for (cref, spec) in c.all_cells(si) {
                if let Some(cell) = spec.to_cell(&mut out.styles) {
                    store.set(cref, cell);
                }
            }
            let features = c.all_features(si);
            let sh = Arc::make_mut(&mut out.sheets[si]);
            sh.cells = store;
            for f in features {
                set_feature(sh, f);
            }
        }
        Ok((c, out))
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

    /// Where the sheet with this key is here now.
    pub(crate) fn sheet_uid(&self, key: &str) -> Option<u64> {
        self.sheets.iter().find(|m| m.key == key).map(|m| m.uid)
    }

    /// The sheet with this key now exists here as `uid` (a sheet someone else added).
    pub(crate) fn set_sheet_uid(&mut self, key: &str, uid: u64) {
        if let Some(m) = self.sheets.iter_mut().find(|m| m.key == key) {
            m.uid = uid;
        }
    }

    /// A new sheet's line lists (its used rows and columns plus a margin), in the mirror.
    fn new_sheet_mirror(&mut self, key: String, sh: &Sheet) -> Result<SheetMirror> {
        let used = sh.cells.used_range();
        let nrows = used.map_or(0, |u| u.end.row + 1).saturating_add(ROW_MARGIN).max(MIN_ROWS).min(MAX_ROWS);
        let ncols = used.map_or(0, |u| u.end.col + 1).saturating_add(COL_MARGIN).max(MIN_COLS).min(MAX_COLS);
        let row_ids: Vec<String> = (0..nrows).map(|r| format!("r{r}")).collect();
        let col_ids: Vec<String> = (0..ncols).map(|k| format!("c{k}")).collect();
        for (axis, ids) in [(Axis::Rows, &row_ids), (Axis::Cols, &col_ids)] {
            let l = self.doc.get_movable_list(lines_name(&key, axis).as_str());
            for id in ids {
                l.push(id.as_str()).map_err(err)?;
            }
        }
        Ok(SheetMirror { key, uid: sh.uid.get(), rows: Lines::new(row_ids), cols: Lines::new(col_ids) })
    }

    /// Writes what changed between the workbook the mirror matched and `after` into the mirror,
    /// and queues the update for the others.
    pub fn capture(&mut self, after: &Arc<Workbook>) -> Result<()> {
        if Arc::ptr_eq(&self.base, after) {
            self.pending.clear();
            return Ok(());
        }
        let fresh = self.sync_sheet_list(after)?;
        let edits = std::mem::take(&mut self.pending);
        let mut reshaped: HashSet<u64> = HashSet::new();
        for (name, e) in &edits {
            let Some(si) = after.sheet_index(name) else { continue };
            if si >= self.sheets.len() || fresh.contains(&self.sheets[si].uid) {
                continue;
            }
            match *e {
                Edit::Insert { axis, at, count } => self.insert_lines(si, axis, at, count)?,
                Edit::Delete { axis, at, count } => self.delete_lines(si, axis, at, count)?,
                _ => continue,
            }
            reshaped.insert(self.sheets[si].uid);
        }
        let base = self.base.clone();
        let base_by_uid: HashMap<u64, &Arc<Sheet>> = base.sheets.iter().map(|s| (s.uid.get(), s)).collect();
        for si in 0..after.sheets.len().min(self.sheets.len()) {
            let a = &after.sheets[si];
            let uid = a.uid.get();
            let b = base_by_uid.get(&uid).copied();
            if b.is_some_and(|b| Arc::ptr_eq(b, a)) {
                continue;
            }
            match b.filter(|_| !reshaped.contains(&uid) && !fresh.contains(&uid)) {
                Some(b) => {
                    for c in b.cells.diff(&a.cells) {
                        self.write_cell(si, c, a.cells.get(c), after)?;
                    }
                    self.sync_features(si, a, Some(b))?;
                }
                None => self.sync_sheet(si, after)?,
            }
        }
        self.base = after.clone();
        self.flush()
    }

    /// Brings the mirror's sheets in line with the workbook's (gone, moved, new and renamed
    /// ones); the uids of the new ones.
    fn sync_sheet_list(&mut self, after: &Workbook) -> Result<HashSet<u64>> {
        let order = self.doc.get_movable_list(SHEET_ORDER);
        let uids: Vec<u64> = after.sheets.iter().map(|s| s.uid.get()).collect();
        let mut i = self.sheets.len();
        while i > 0 {
            i -= 1;
            if !uids.contains(&self.sheets[i].uid) {
                order.delete(i, 1).map_err(err)?;
                self.sheets.remove(i);
            }
        }
        let mut fresh = HashSet::new();
        for (ai, sh) in after.sheets.iter().enumerate() {
            let uid = sh.uid.get();
            match self.sheets.iter().position(|m| m.uid == uid) {
                Some(mi) if mi == ai => {}
                Some(mi) => {
                    order.mov(mi, ai).map_err(err)?;
                    let m = self.sheets.remove(mi);
                    self.sheets.insert(ai, m);
                }
                None => {
                    self.next_id += 1;
                    let key = format!("s{:x}.{}", self.doc.peer_id(), self.next_id);
                    order.insert(ai, key.as_str()).map_err(err)?;
                    let m = self.new_sheet_mirror(key, sh)?;
                    self.sheets.insert(ai, m);
                    fresh.insert(uid);
                }
            }
            let meta = self.doc.get_map(part(&self.sheets[ai].key, "meta").as_str());
            put(&meta, "name", Some(sh.name.clone()))?;
        }
        Ok(fresh)
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
        let len = self.sheets[si].lines(axis).ids.len();
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
        let len = self.sheets[si].lines(axis).ids.len();
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
        let map = self.doc.get_map(part(&m.key, "cells").as_str());
        put(&map, &key, cell.and_then(entry))?;
        let style = match cell {
            Some(cell) => self.style_key(wb, cell.style)?,
            None => None,
        };
        put(&map, &format!("{key}{STYLE_SUFFIX}"), style)
    }

    /// Writes all of sheet `si` into the mirror and drops the entries of what is gone (or whose
    /// line was deleted): for a new sheet, and after a line insert or delete.
    fn sync_sheet(&self, si: usize, wb: &Workbook) -> Result<()> {
        let Some(sh) = wb.sheets.get(si) else { return Ok(()) };
        for (c, cell) in sh.cells.iter() {
            self.write_cell(si, c, Some(cell), wb)?;
        }
        let m = &self.sheets[si];
        let map = self.doc.get_map(part(&m.key, "cells").as_str());
        for k in map_keys(&map) {
            let base = k.strip_suffix(STYLE_SUFFIX).unwrap_or(k.as_str());
            if m.parse_key(base).is_none_or(|c| sh.cells.get(c).is_none()) {
                map.delete(&k).map_err(err)?;
            }
        }
        self.sync_features(si, sh, None)
    }

    /// Writes the sheet's other parts that differ from `b` (all of them without `b`).
    fn sync_features(&self, si: usize, a: &Sheet, b: Option<&Sheet>) -> Result<()> {
        let m = &self.sheets[si];
        for (axis, kind, info) in [(Axis::Rows, "rowinfo", &a.rows), (Axis::Cols, "colinfo", &a.cols)] {
            let old = b.map(|b| if axis == Axis::Rows { &b.rows } else { &b.cols });
            if old.is_some_and(|o| o == info) {
                continue;
            }
            let lines = m.lines(axis);
            let map = self.doc.get_map(part(&m.key, kind).as_str());
            let mut keep = HashSet::new();
            for (i, li) in info {
                let token = lines.token(*i);
                put(&map, &token, serde_json::to_string(li).ok())?;
                keep.insert(token);
            }
            for k in map_keys(&map) {
                if !keep.contains(&k) {
                    map.delete(&k).map_err(err)?;
                }
            }
        }
        if b.is_none_or(|b| b.merges != a.merges) {
            let map = self.doc.get_map(part(&m.key, "merges").as_str());
            let keep: HashSet<String> = a.merges.iter().map(|r| m.range_key(*r)).collect();
            for k in &keep {
                put(&map, k, Some("1".into()))?;
            }
            for k in map_keys(&map) {
                if !keep.contains(&k) {
                    map.delete(&k).map_err(err)?;
                }
            }
        }
        if b.is_none_or(|b| b.comments != a.comments) {
            self.sync_cell_map(m, "notes", a.comments.iter().map(|(c, x)| (*c, serde_json::to_string(x).ok())))?;
        }
        if b.is_none_or(|b| b.hyperlinks != a.hyperlinks) {
            self.sync_cell_map(m, "links", a.hyperlinks.iter().map(|(c, x)| (*c, serde_json::to_string(x).ok())))?;
        }
        let settings = changed_settings(b, a);
        if !settings.is_empty() {
            let map = self.doc.get_map(part(&m.key, "settings").as_str());
            for (k, json) in settings {
                put(&map, k, Some(json))?;
            }
        }
        Ok(())
    }

    /// A map of things kept by cell (notes, links): writes these, drops the others.
    fn sync_cell_map(&self, m: &SheetMirror, kind: &str, items: impl Iterator<Item = (CellRef, Option<String>)>) -> Result<()> {
        let map = self.doc.get_map(part(&m.key, kind).as_str());
        let mut keep = HashSet::new();
        for (c, json) in items {
            let k = m.cell_key(c);
            put(&map, &k, json)?;
            keep.insert(k);
        }
        for k in map_keys(&map) {
            if !keep.contains(&k) {
                map.delete(&k).map_err(err)?;
            }
        }
        Ok(())
    }

    fn read_cell(&self, si: usize, key: &str) -> CellSpec {
        let map = self.doc.get_map(part(&self.sheets[si].key, "cells").as_str());
        let styles = self.doc.get_map("styles");
        CellSpec { entry: get_str(&map, key), style: get_str(&map, &format!("{key}{STYLE_SUFFIX}")).and_then(|h| get_str(&styles, &h)) }
    }

    /// Every cell of sheet `si` the mirror holds, where it is now.
    fn all_cells(&self, si: usize) -> Vec<(CellRef, CellSpec)> {
        let m = &self.sheets[si];
        let map = self.doc.get_map(part(&m.key, "cells").as_str());
        let keys: BTreeSet<String> = map_keys(&map).into_iter().map(|k| k.strip_suffix(STYLE_SUFFIX).unwrap_or(&k).to_string()).collect();
        keys.into_iter().filter_map(|k| Some((m.parse_key(&k)?, self.read_cell(si, &k)))).collect()
    }

    /// One part of sheet `si` as the mirror holds it.
    fn feature(&self, si: usize, kind: &str) -> Vec<Feature> {
        let m = &self.sheets[si];
        let map = self.doc.get_map(part(&m.key, kind).as_str());
        let entries = || map_keys(&map).into_iter().filter_map(|k| get_str(&map, &k).map(|v| (k, v)));
        match kind {
            "rowinfo" | "colinfo" => {
                let lines = if kind == "rowinfo" { &m.rows } else { &m.cols };
                let info: BTreeMap<u32, LineInfo> =
                    entries().filter_map(|(k, v)| Some((lines.find(&k)?, serde_json::from_str(&v).ok()?))).collect();
                vec![if kind == "rowinfo" { Feature::Rows(info) } else { Feature::Cols(info) }]
            }
            "merges" => {
                let mut merges: Vec<RangeRef> = map_keys(&map).iter().filter_map(|k| m.parse_range(k)).collect();
                merges.sort_by_key(|r| (r.start.row, r.start.col));
                vec![Feature::Merges(merges)]
            }
            "notes" => vec![Feature::Comments(entries().filter_map(|(k, v)| Some((m.parse_key(&k)?, serde_json::from_str(&v).ok()?))).collect())],
            "links" => vec![Feature::Links(entries().filter_map(|(k, v)| Some((m.parse_key(&k)?, serde_json::from_str(&v).ok()?))).collect())],
            "settings" => entries().map(|(k, v)| Feature::Setting(k, v)).collect(),
            _ => vec![],
        }
    }

    /// Every part of sheet `si` the mirror holds.
    fn all_features(&self, si: usize) -> Vec<Feature> {
        ["rowinfo", "colinfo", "merges", "notes", "links", "settings"].iter().flat_map(|k| self.feature(si, k)).collect()
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
        self.changes(&before, &after)
    }

    /// Whether there is a change of this person's to undo, or an undone one to redo.
    pub fn can_undo(&self, redo: bool) -> bool {
        let u = self.undo.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if redo { u.can_redo() } else { u.can_undo() }
    }

    /// Undoes (or redoes) this person's last change, adjusted to what the others did since, and
    /// queues it for the others. `None` when there was nothing to undo.
    pub fn undo(&mut self, redo: bool) -> Result<Option<Remote>> {
        let before = self.doc.state_frontiers();
        let done = {
            let mut u = self.undo.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if redo { u.redo() } else { u.undo() }
        }
        .map_err(err)?;
        if !done {
            return Ok(None);
        }
        self.doc.commit();
        let after = self.doc.state_frontiers();
        let remote = self.changes(&before, &after)?;
        self.flush()?;
        Ok(Some(remote))
    }

    /// The sheets as the mirror now has them, against the mirror's list: the plan for the
    /// workbook, and the keys of the sheets that are new here.
    fn plan_sheets(&mut self) -> (SheetPlan, HashSet<String>) {
        let keys = movable_strings(&self.doc.get_movable_list(SHEET_ORDER));
        let mut old: HashMap<String, SheetMirror> = self.sheets.drain(..).map(|m| (m.key.clone(), m)).collect();
        let mut plan = SheetPlan::default();
        let mut fresh = HashSet::new();
        for key in keys {
            let name = get_str(&self.doc.get_map(part(&key, "meta").as_str()), "name").unwrap_or_else(|| key.clone());
            let m = match old.remove(&key) {
                Some(m) => m,
                None => {
                    fresh.insert(key.clone());
                    let rows = Lines::new(movable_strings(&self.doc.get_movable_list(lines_name(&key, Axis::Rows).as_str())));
                    let cols = Lines::new(movable_strings(&self.doc.get_movable_list(lines_name(&key, Axis::Cols).as_str())));
                    SheetMirror { key: key.clone(), uid: 0, rows, cols }
                }
            };
            plan.order.push((key, m.uid, name));
            self.sheets.push(m);
        }
        plan.deleted = old.into_values().map(|m| m.uid).filter(|u| *u != 0).collect();
        (plan, fresh)
    }

    /// What changed in the mirror between two of its versions, in the workbook's terms.
    fn changes(&mut self, before: &Frontiers, after: &Frontiers) -> Result<Remote> {
        let mut remote = Remote::default();
        if before == after {
            return Ok(remote);
        }
        let batch = self.doc.diff(before, after).map_err(err)?;
        // Sheets first: everything else is found by the sheets' keys.
        let mut fresh = HashSet::new();
        if batch.iter().any(|(cid, _)| matches!(cid, ContainerID::Root { name, .. } if &**name == SHEET_ORDER)) {
            let (plan, new) = self.plan_sheets();
            fresh = new;
            remote.sheets = Some(plan);
        }
        let mut cell_keys: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        let mut parts: BTreeSet<(usize, &'static str)> = BTreeSet::new();
        let mut moved: BTreeSet<usize> = BTreeSet::new();
        for (cid, diff) in batch.iter() {
            let ContainerID::Root { name, .. } = cid else { continue };
            let name: &str = name.as_ref();
            let Some((key, kind)) = name.rsplit_once(':') else { continue };
            if fresh.contains(key) {
                continue;
            }
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
                ("meta", Diff::Map(delta)) => {
                    if delta.updated.contains_key("name")
                        && let Some(n) = get_str(&self.doc.get_map(name), "name")
                    {
                        remote.renames.push((si, n));
                    }
                }
                ("rowinfo", _) => {
                    parts.insert((si, "rowinfo"));
                }
                ("colinfo", _) => {
                    parts.insert((si, "colinfo"));
                }
                ("merges", _) => {
                    parts.insert((si, "merges"));
                }
                ("notes", _) => {
                    parts.insert((si, "notes"));
                }
                ("links", _) => {
                    parts.insert((si, "links"));
                }
                ("settings", Diff::Map(delta)) => {
                    let map = self.doc.get_map(name);
                    for k in delta.updated.keys() {
                        if let Some(v) = get_str(&map, k.as_ref()) {
                            remote.features.push((si, Feature::Setting(k.to_string(), v)));
                        }
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
                for kind in ["rowinfo", "colinfo", "merges", "notes", "links"] {
                    parts.insert((si, kind));
                }
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
        for (si, kind) in parts {
            for f in self.feature(si, kind) {
                remote.features.push((si, f));
            }
        }
        // A sheet new here comes whole from the mirror.
        for si in 0..self.sheets.len() {
            if fresh.contains(&self.sheets[si].key) {
                remote.rebuilt.push((si, self.all_cells(si)));
                for f in self.all_features(si) {
                    remote.features.push((si, f));
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

/// Sets a part of a sheet from the mirror (on a workbook being joined).
fn set_feature(sh: &mut Sheet, f: Feature) {
    match f {
        Feature::Rows(rows) => sh.rows = rows,
        Feature::Cols(cols) => sh.cols = cols,
        Feature::Merges(merges) => sh.merges = merges,
        Feature::Comments(comments) => sh.comments = comments,
        Feature::Links(links) => sh.hyperlinks = links,
        Feature::Setting(key, json) => apply_setting(sh, &key, &json),
    }
}

fn new_undo(doc: &LoroDoc) -> Arc<std::sync::Mutex<UndoManager>> {
    let mut u = UndoManager::new(doc);
    u.set_max_undo_steps(100);
    Arc::new(std::sync::Mutex::new(u))
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

    /// Joins a shared workbook from its snapshot. The active workbook should be the same file;
    /// its sheets and cells are replaced by the shared ones.
    pub fn collab_join(&mut self, snapshot: &[u8]) -> Result<()> {
        let d = self.doc_mut()?;
        let (mut c, mut wb) = Collab::join(snapshot, &d.wb)?;
        let mut calc = gridcraft_calc::Calc::new();
        calc.recalc_all(&mut wb);
        let n = wb.sheets.len();
        let wb = Arc::new(wb);
        c.rebase(&wb);
        d.calc = calc;
        d.wb = wb.clone();
        d.saved = wb;
        d.undo.clear();
        d.redo.clear();
        d.sheet_selections = vec![crate::Selection::default(); n];
        d.revision += 1;
        d.collab = Some(Box::new(c));
        Ok(())
    }

    /// Applies an update from someone else to the active workbook.
    pub fn collab_receive(&mut self, update: &[u8]) -> Result<()> {
        let d = self.doc_mut()?;
        let Some(c) = d.collab.as_mut() else { return Err(EngineError::Other("co-editing: this workbook is not shared".into())) };
        let remote = c.receive(update)?;
        self.collab_apply(remote)
    }

    /// Runs commands for changes someone else made (sheets added, moved, renamed, deleted)
    /// without writing them back into the mirror, which already has them.
    fn collab_quietly(&mut self, f: impl FnOnce(&mut Session)) {
        self.collab_quiet = true;
        f(self);
        self.collab_quiet = false;
        if let Some(d) = self.active_mut()
            && let Some(c) = d.collab.as_mut()
        {
            c.rebase(&d.wb);
        }
    }

    /// Where the sheet with this uid is in the active workbook.
    fn sheet_at(&self, uid: u64) -> Option<usize> {
        self.active()?.wb.sheets.iter().position(|s| s.uid.get() == uid)
    }

    /// Makes the workbook's sheets what someone else left them: deletes, adds and moves sheets
    /// with the sheet commands, then puts this person back on the sheet they were on.
    fn collab_apply_sheets(&mut self, plan: &SheetPlan) {
        let Some(d) = self.active() else { return };
        let here = d.wb.sheets.get(d.wb.active_sheet()).map(|s| s.uid.get());
        let selection = d.selection.clone();
        self.collab_quietly(|s| {
            for uid in &plan.deleted {
                if let Some(i) = s.sheet_at(*uid)
                    && let Err(e) = s.execute("home.deleteSheet", serde_json::json!({ "sheet": i }))
                {
                    log::warn!("co-editing: couldn't delete a sheet: {e}");
                }
            }
            for (target, (key, uid, name)) in plan.order.iter().enumerate() {
                if *uid == 0 {
                    // Two people adding a sheet with the same name at once: this one gets a
                    // name of its own here (and the next change of either brings the names in line).
                    let taken = s.active().is_some_and(|d| d.wb.sheet_index(name).is_some());
                    let name = if taken { format!("{name} ({})", target + 1) } else { name.clone() };
                    match s.execute("home.insertSheet", serde_json::json!({ "name": name, "before": target })) {
                        Ok(_) => {
                            if let Some(d) = s.active_mut()
                                && let Some(new) = d.wb.sheets.get(target).map(|x| x.uid.get())
                                && let Some(c) = d.collab.as_mut()
                            {
                                c.set_sheet_uid(key, new);
                            }
                        }
                        Err(e) => log::warn!("co-editing: couldn't add sheet {name}: {e}"),
                    }
                } else if let Some(cur) = s.sheet_at(*uid)
                    && cur != target
                    && let Err(e) = s.execute("sheet.move", serde_json::json!({ "sheet": cur, "to": target }))
                {
                    log::warn!("co-editing: couldn't move a sheet: {e}");
                }
            }
        });
        // Adding a sheet goes to it; this person stays where they were.
        if let Some(i) = here.and_then(|u| self.sheet_at(u))
            && let Some(d) = self.active_mut()
        {
            if d.wb.active_sheet() != i {
                Arc::make_mut(&mut d.wb).set_active_sheet(i);
            }
            d.selection = selection;
            if let Some(c) = d.collab.as_mut() {
                c.rebase(&d.wb);
            }
        }
    }

    /// Applies what someone else changed (or what Undo took back) to the active workbook.
    fn collab_apply(&mut self, mut remote: Remote) -> Result<()> {
        if let Some(plan) = remote.sheets.take() {
            self.collab_apply_sheets(&plan);
        }
        let renames = std::mem::take(&mut remote.renames);
        if !renames.is_empty() {
            self.collab_quietly(|s| {
                for (si, name) in renames {
                    let same = s.active().and_then(|d| d.wb.sheets.get(si)).is_none_or(|sh| sh.name == name);
                    if !same && let Err(e) = s.execute("sheet.rename", serde_json::json!({ "sheet": si, "name": name })) {
                        log::warn!("co-editing: couldn't rename a sheet: {e}");
                    }
                }
            });
        }
        let d = self.doc_mut()?;
        if remote.is_empty() {
            d.revision += 1;
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

    /// Undo (or Redo) in a shared workbook: this person's own last `steps` changes, applied to the
    /// workbook and sent to the others.
    pub fn collab_undo(&mut self, redo: bool, steps: usize) -> Result<Json> {
        let mut done = 0;
        for _ in 0..steps.max(1) {
            let d = self.doc_mut()?;
            let Some(c) = d.collab.as_mut() else { break };
            let Some(remote) = c.undo(redo)? else { break };
            let first = remote.cells.first().map(|(si, c, _)| (*si, *c));
            self.collab_apply(remote)?;
            // The cell that changed back comes into view, as Undo shows it in Excel.
            if let Some((si, cell)) = first
                && let Some(d) = self.active_mut()
                && si == d.wb.active_sheet()
            {
                d.selection = crate::Selection::at(cell);
            }
            done += 1;
        }
        if done == 0 {
            let id = if redo { "edit.redo" } else { "edit.undo" };
            let why = if redo { "nothing to redo" } else { "nothing to undo" };
            return Err(EngineError::Disabled(id.into(), why.into()));
        }
        Ok(serde_json::json!({ "steps": done }))
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
        self.collab_peers.clear();
    }

    /// Where this person is in the shared workbook, for the others: the sheet (by position), the
    /// selected areas and the active cell. `None` when the active workbook is not shared.
    pub fn collab_presence(&self) -> Option<(usize, Vec<String>, String)> {
        let d = self.active().filter(|d| d.collab.is_some())?;
        Some((d.wb.active_sheet(), d.selection.ranges.iter().map(|r| r.a1()).collect(), d.selection.active.a1()))
    }

    /// After a top-level command: what it changed goes into the mirror (unless it was someone
    /// else's change being made here).
    pub(crate) fn collab_capture(&mut self, uid: u64) {
        let quiet = self.collab_quiet;
        if let Some(d) = self.docs.iter_mut().find(|d| d.uid == uid)
            && let Some(c) = d.collab.as_mut()
        {
            if quiet {
                c.rebase(&d.wb);
            } else if let Err(e) = c.capture(&d.wb) {
                log::warn!("{e}");
            }
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

    fn shown_on(s: &Session, sheet: &str, cell: &str) -> Value {
        let wb = &s.doc().unwrap().wb;
        let c = CellRef::parse(cell).unwrap();
        wb.sheets[wb.sheet_index(sheet).unwrap()].cells.get(c).map(|c| c.value.clone()).unwrap_or(Value::Empty)
    }

    fn formula(s: &Session, cell: &str) -> Option<String> {
        let c = CellRef::parse(cell).unwrap();
        s.doc().unwrap().wb.sheets[0].cells.get(c).and_then(|c| c.formula.as_ref().map(|f| f.text.clone()))
    }

    fn sheet_names(s: &Session) -> Vec<String> {
        s.doc().unwrap().wb.sheets.iter().map(|x| x.name.clone()).collect()
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
    fn undo_takes_back_only_your_own_change() {
        let (mut a, mut b) = pair();
        set(&mut a, "B1", "de A");
        set(&mut b, "C1", "de B");
        pass(&mut a, &mut b);
        pass(&mut b, &mut a);
        // A undoes: B1 goes for both, C1 (B's) stays.
        a.execute("edit.undo", json!({})).unwrap();
        pass(&mut a, &mut b);
        for s in [&a, &b] {
            assert_eq!(shown(s, "B1"), Value::Empty);
            assert_eq!(shown(s, "C1"), Value::Text("de B".into()));
        }
        // And redoes it.
        a.execute("edit.redo", json!({})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(shown(&b, "B1"), Value::Text("de A".into()));
    }

    #[test]
    fn undo_puts_back_the_value_before_your_change() {
        let (mut a, mut b) = pair();
        set(&mut a, "A1", "8");
        pass(&mut a, &mut b);
        a.execute("edit.undo", json!({})).unwrap();
        pass(&mut a, &mut b);
        for s in [&a, &b] {
            assert_eq!(shown(s, "A1"), Value::Number(5.0));
            assert_eq!(shown(s, "A2"), Value::Number(10.0));
        }
    }

    #[test]
    fn undo_of_an_inserted_row_removes_it_for_both() {
        let (mut a, mut b) = pair();
        a.execute("home.insertRows", json!({"rows": "1:1"})).unwrap();
        pass(&mut a, &mut b);
        a.execute("edit.undo", json!({})).unwrap();
        pass(&mut a, &mut b);
        for s in [&a, &b] {
            assert_eq!(shown(s, "A1"), Value::Number(5.0));
            assert_eq!(formula(s, "A2").as_deref(), Some("A1*2"));
        }
    }

    #[test]
    fn nothing_to_undo_before_your_first_change() {
        let (_, mut b) = pair();
        assert!(b.execute("edit.undo", json!({})).is_err());
    }

    #[test]
    fn sheets_added_renamed_moved_and_deleted_reach_the_other() {
        let (mut a, mut b) = pair();
        a.execute("home.insertSheet", json!({"name": "Nova", "before": 1})).unwrap();
        a.execute("cell.set", json!({"cell": "Nova!A1", "input": "na nova"})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(sheet_names(&b), sheet_names(&a));
        assert_eq!(shown_on(&b, "Nova", "A1"), Value::Text("na nova".into()));
        // B stays on the sheet it was on.
        assert_eq!(b.doc().unwrap().wb.active_sheet(), 0);
        // B types on the new sheet; A sees it.
        b.execute("cell.set", json!({"cell": "Nova!B2", "input": "7"})).unwrap();
        pass(&mut b, &mut a);
        assert_eq!(shown_on(&a, "Nova", "B2"), Value::Number(7.0));
        a.execute("sheet.rename", json!({"sheet": "Nova", "name": "Renomeada"})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(sheet_names(&b), sheet_names(&a));
        a.execute("sheet.move", json!({"sheet": "Renomeada", "to": 0})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(sheet_names(&b), sheet_names(&a));
        assert_eq!(sheet_names(&b)[0], "Renomeada");
        a.execute("home.deleteSheet", json!({"sheet": "Renomeada"})).unwrap();
        pass(&mut a, &mut b);
        assert_eq!(sheet_names(&b), sheet_names(&a));
        assert_eq!(shown(&b, "A1"), Value::Number(5.0));
    }

    #[test]
    fn widths_merges_and_notes_reach_the_other() {
        let (mut a, mut b) = pair();
        a.execute("home.columnWidth", json!({"cols": "B:B", "width": 120})).unwrap();
        a.execute("home.mergeCells", json!({"range": "D1:E2"})).unwrap();
        a.execute("review.newNote", json!({"cell": "C3", "text": "uma nota"})).unwrap();
        pass(&mut a, &mut b);
        {
            let (sa, sb) = (&a.doc().unwrap().wb.sheets[0], &b.doc().unwrap().wb.sheets[0]);
            assert!(!sa.cols.is_empty() && !sa.merges.is_empty() && !sa.comments.is_empty());
            assert_eq!(sb.cols, sa.cols);
            assert_eq!(sb.merges, sa.merges);
            assert_eq!(sb.comments.keys().collect::<Vec<_>>(), sa.comments.keys().collect::<Vec<_>>());
        }
        // A row inserted by B moves the merge and the note for both.
        b.execute("home.insertRows", json!({"rows": "1:1"})).unwrap();
        pass(&mut b, &mut a);
        let (sa, sb) = (&a.doc().unwrap().wb.sheets[0], &b.doc().unwrap().wb.sheets[0]);
        assert_eq!(sb.merges, sa.merges);
        assert_eq!(sa.merges[0].start.row, 1);
        assert_eq!(sb.comments.keys().collect::<Vec<_>>(), sa.comments.keys().collect::<Vec<_>>());
    }

    #[test]
    fn a_frozen_pane_reaches_the_other() {
        let (mut a, mut b) = pair();
        a.execute("view.freezeTopRow", json!({})).unwrap();
        pass(&mut a, &mut b);
        assert!(a.doc().unwrap().wb.sheets[0].freeze.is_some());
        assert_eq!(b.doc().unwrap().wb.sheets[0].freeze, a.doc().unwrap().wb.sheets[0].freeze);
    }

    #[test]
    fn joining_after_a_sheet_was_added_takes_it_too() {
        let mut a = session();
        set(&mut a, "A1", "5");
        a.collab_seed().unwrap();
        a.execute("home.insertSheet", json!({"name": "Extra", "before": 1})).unwrap();
        a.execute("cell.set", json!({"cell": "Extra!A1", "input": "x"})).unwrap();
        let _ = a.collab_outbox();
        let snapshot = a.collab_snapshot().unwrap();
        let mut b = session();
        b.collab_join(&snapshot).unwrap();
        assert_eq!(sheet_names(&b), sheet_names(&a));
        assert_eq!(shown_on(&b, "Extra", "A1"), Value::Text("x".into()));
    }
}
