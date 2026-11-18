//! Verto: the values of other workbooks that formulas here point at (`'[Book.xlsx]Sheet'!A1`),
//! kept with the workbook while it is open and never saved. The host (Verto) hands them over
//! as values only; their sheets take indexes from [`LINKED_BASE`] up, so the calculation reads
//! them as it reads the workbook's own sheets. References the calculation meets are noted, so the
//! host can fetch the blocks they need ([`LinkedBooks::take_wanted`]).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use gridcraft_core::RangeRef;

use crate::sheet::Sheet;

/// The index of the first linked sheet: far beyond any workbook's own sheets.
pub const LINKED_BASE: usize = 1 << 24;

/// A sheet of another workbook, values only. The sheet's own name is the name inside its book.
#[derive(Clone, Debug)]
pub struct LinkedSheet {
    pub book: String,
    pub sheet: Arc<Sheet>,
}

/// One block a formula reads from another workbook: book, sheet, `A1:B2`.
pub type Wanted = (String, String, String);

#[derive(Default)]
struct WantedSet {
    seen: BTreeSet<Wanted>,
    fresh: Vec<Wanted>,
}

/// The other workbooks' sheets, and the blocks formulas asked for. Copies of a workbook (every
/// version) share the wanted list.
#[derive(Clone, Default)]
pub struct LinkedBooks {
    sheets: Arc<Vec<LinkedSheet>>,
    wanted: Arc<Mutex<WantedSet>>,
}

/// `[Book.xlsx]Sheet` → (`Book.xlsx`, `Sheet`).
pub fn split(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix('[')?;
    let (book, sheet) = rest.split_once(']')?;
    (!book.trim().is_empty() && !sheet.is_empty()).then_some((book, sheet))
}

impl LinkedBooks {
    /// Replaces the sheets (the wanted list stays).
    pub fn set(&mut self, sheets: Vec<LinkedSheet>) {
        self.sheets = Arc::new(sheets);
    }
    pub fn is_empty(&self) -> bool {
        self.sheets.is_empty()
    }
    /// The linked sheet at `i` (counted from [`LINKED_BASE`]).
    pub fn sheet(&self, i: usize) -> Option<&Sheet> {
        self.sheets.get(i).map(|l| l.sheet.as_ref())
    }
    /// The position of `[Book]Sheet` among the linked sheets (case-insensitive).
    pub fn index(&self, name: &str) -> Option<usize> {
        let (book, sheet) = split(name)?;
        self.sheets.iter().position(|l| l.book.eq_ignore_ascii_case(book.trim()) && l.sheet.name.eq_ignore_ascii_case(sheet))
    }
    /// Notes that a formula reads `range` of `[Book]Sheet`.
    pub fn note(&self, name: &str, range: RangeRef) {
        let Some((book, sheet)) = split(name) else { return };
        let key = (book.trim().to_string(), sheet.to_string(), range.a1());
        let mut w = self.wanted.lock().unwrap_or_else(|e| e.into_inner());
        if w.seen.insert(key.clone()) {
            w.fresh.push(key);
        }
    }
    /// The blocks noted since the last call.
    pub fn take_wanted(&self) -> Vec<Wanted> {
        std::mem::take(&mut self.wanted.lock().unwrap_or_else(|e| e.into_inner()).fresh)
    }
    /// Every block noted so far (to ask again, e.g. after the host lost them).
    pub fn all_wanted(&self) -> Vec<Wanted> {
        self.wanted.lock().unwrap_or_else(|e| e.into_inner()).seen.iter().cloned().collect()
    }
}

/// Linked values are not part of the workbook's content: two workbooks equal otherwise are equal.
impl PartialEq for LinkedBooks {
    fn eq(&self, _: &LinkedBooks) -> bool {
        true
    }
}

impl std::fmt::Debug for LinkedBooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinkedBooks({} sheets)", self.sheets.len())
    }
}
