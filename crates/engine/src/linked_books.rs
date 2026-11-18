//! Verto: other workbooks' values for the formulas that point at them (`'[Book.xlsx]Sheet'!A1`).
//! The calculation notes the blocks it reads ([`Session::linked_wanted`]); the host fetches them
//! and hands the values over ([`Session::set_linked`]), and the workbooks are recalculated.

use std::sync::Arc;

use gridcraft_core::{CellError, CellRef, Value};
use gridcraft_model::linked::{LinkedSheet, Wanted};
use gridcraft_model::{Cell, Sheet};
use serde_json::Value as Json;

use crate::Session;

impl Session {
    /// The blocks of other workbooks that formulas asked for since the last call (book, sheet,
    /// `A1:B2`), for the host to fetch.
    pub fn linked_wanted(&self) -> Vec<Wanted> {
        let mut out: Vec<Wanted> = Vec::new();
        for d in &self.docs {
            for w in d.wb.linked.take_wanted() {
                if !out.contains(&w) {
                    out.push(w);
                }
            }
        }
        out
    }

    /// Hands over the values of other workbooks: `[{"book", "sheet", "cells": [[row, col,
    /// value], …]}]`, rows and columns from 0, a value a number, text, true/false or `{"e":
    /// "#N/A"}`. Replaces what was there, and recalculates every open workbook. A workbook that
    /// had no unsaved changes still has none: the values are not part of it.
    pub fn set_linked(&mut self, json: &str) -> Result<(), String> {
        let books: Vec<Json> = serde_json::from_str(json).map_err(|e| e.to_string())?;
        let mut sheets = Vec::with_capacity(books.len());
        for b in &books {
            let book = b.get("book").and_then(Json::as_str).unwrap_or_default().trim().to_string();
            let name = b.get("sheet").and_then(Json::as_str).unwrap_or_default();
            if book.is_empty() || name.is_empty() {
                continue;
            }
            let mut sheet = Sheet::new(name);
            for c in b.get("cells").and_then(Json::as_array).into_iter().flatten() {
                let (Some(row), Some(col)) = (c.get(0).and_then(Json::as_u64), c.get(1).and_then(Json::as_u64)) else { continue };
                let value = match c.get(2) {
                    Some(Json::Number(n)) => n.as_f64().map(Value::Number).unwrap_or(Value::Empty),
                    Some(Json::String(s)) => Value::Text(s.as_str().into()),
                    Some(Json::Bool(v)) => Value::Bool(*v),
                    Some(Json::Object(o)) => o.get("e").and_then(Json::as_str).and_then(CellError::parse).map(Value::Error).unwrap_or(Value::Empty),
                    _ => Value::Empty,
                };
                if !value.is_empty() {
                    sheet.cells.set(CellRef::new(row as u32, col as u32), Cell::value(value));
                }
            }
            sheets.push(LinkedSheet { book, sheet: Arc::new(sheet) });
        }
        for d in &mut self.docs {
            let clean = !d.is_dirty();
            let mut wb = (*d.wb).clone();
            wb.linked.set(sheets.clone());
            d.calc.recalc_all(&mut wb);
            d.wb = Arc::new(wb);
            if clean {
                d.saved = d.wb.clone();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn shown(s: &Session, cell: &str) -> Value {
        let c = CellRef::parse(cell).unwrap();
        s.doc().unwrap().wb.sheets[0].cells.get(c).map(|c| c.value.clone()).unwrap_or(Value::Empty)
    }

    #[test]
    fn formulas_read_another_workbook_once_its_values_arrive() {
        let mut s = Session::new();
        s.new_workbook();
        s.execute("cell.set", json!({"cell": "A1", "input": "='[Orçamento.xlsx]Resumo'!B2*2"})).unwrap();
        s.execute("cell.set", json!({"cell": "A2", "input": "=SUM('[Orçamento.xlsx]Resumo'!B2:B3)"})).unwrap();
        assert_eq!(shown(&s, "A1"), Value::Error(CellError::Ref), "no values yet");
        let mut wanted = s.linked_wanted();
        wanted.sort();
        assert_eq!(
            wanted,
            vec![
                ("Orçamento.xlsx".to_string(), "Resumo".to_string(), "B2".to_string()),
                ("Orçamento.xlsx".to_string(), "Resumo".to_string(), "B2:B3".to_string()),
            ]
        );
        assert!(s.linked_wanted().is_empty(), "each block is asked for once");
        let was_dirty = s.doc().unwrap().is_dirty();
        s.set_linked(&json!([{"book": "Orçamento.xlsx", "sheet": "Resumo", "cells": [[1, 1, 21], [2, 1, 4], [0, 0, "Total"]]}]).to_string())
            .unwrap();
        assert_eq!(shown(&s, "A1"), Value::Number(42.0));
        assert_eq!(shown(&s, "A2"), Value::Number(25.0));
        assert_eq!(s.doc().unwrap().is_dirty(), was_dirty, "the linked values don't change whether it is saved");
        // New values from the other workbook recalculate.
        s.set_linked(&json!([{"book": "orçamento.xlsx", "sheet": "resumo", "cells": [[1, 1, 50]]}]).to_string()).unwrap();
        assert_eq!(shown(&s, "A1"), Value::Number(100.0));
        // The workbook's own sheets don't see the linked ones.
        assert_eq!(s.doc().unwrap().wb.sheets.len(), 1);
    }
}
