//! Brazilian regional settings (Verto): numbers shown and typed with a decimal comma and a dot
//! between thousands (`1.234,56`), month and weekday names in Portuguese, days before months in
//! typed dates (`17/11/2026`), and `TEXT` format codes read as pt-BR Excel reads them (`AA` is the
//! year: `"MMM/AA"`). Off by default; the app turns it on with the Portuguese interface.
//!
//! Files keep Excel's invariant form either way: numbers are numbers and number formats keep
//! their English codes, so a workbook saved here opens the same in any Excel.

use std::sync::atomic::{AtomicBool, Ordering};

static PT_BR: AtomicBool = AtomicBool::new(false);

/// Turns the Brazilian settings on or off.
pub fn set_pt_br(on: bool) {
    PT_BR.store(on, Ordering::Relaxed);
}

/// Whether the Brazilian settings are on.
pub fn pt_br() -> bool {
    PT_BR.load(Ordering::Relaxed)
}

/// The decimal separator numbers are shown with.
pub fn decimal_separator() -> char {
    if pt_br() { ',' } else { '.' }
}

/// The separator shown between groups of thousands.
pub fn group_separator() -> char {
    if pt_br() { '.' } else { ',' }
}

/// Month names (`mmmm`).
pub const MONTHS_PT: [&str; 12] =
    ["janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro", "outubro", "novembro", "dezembro"];
/// Month abbreviations (`mmm`), as pt-BR Excel shows them.
pub const MONTHS_PT_SHORT: [&str; 12] = ["jan", "fev", "mar", "abr", "mai", "jun", "jul", "ago", "set", "out", "nov", "dez"];
/// Weekday names (`dddd`), Sunday first.
pub const WEEKDAYS_PT: [&str; 7] = ["domingo", "segunda-feira", "terça-feira", "quarta-feira", "quinta-feira", "sexta-feira", "sábado"];
/// Weekday abbreviations (`ddd`), Sunday first.
pub const WEEKDAYS_PT_SHORT: [&str; 7] = ["dom", "seg", "ter", "qua", "qui", "sex", "sáb"];

/// A number shown in the formula bar or the cell editor: Rust's `1234.5` as `1234,5`.
pub fn localize_number_text(s: &str) -> String {
    if pt_br() { s.replace('.', ",") } else { s.to_string() }
}

/// Typed text in Brazilian form turned into the form the (en-US) input parser reads, or `None`
/// when it has nothing Brazilian to turn: `1.234,56` → `1,234.56`, `R$ 5,50` → `$ 5.50`,
/// `17/11/2026` → `11/17/2026` (`17/11/2026 10:30` keeps its time).
pub fn typed_to_invariant(t: &str) -> Option<String> {
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    // Dates: day first. `d/m`, `d/m/y`, and a time after a space.
    if t.contains('/') {
        let (date, time) = match t.split_once(' ') {
            Some((d, rest)) => (d, Some(rest)),
            None => (t, None),
        };
        let parts: Vec<&str> = date.split('/').collect();
        if (2..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
            let mut swapped = format!("{}/{}", parts[1], parts[0]);
            if let Some(y) = parts.get(2) {
                swapped.push('/');
                swapped.push_str(y);
            }
            if let Some(time) = time {
                swapped.push(' ');
                swapped.push_str(time);
            }
            return Some(swapped);
        }
        return None;
    }
    // Numbers: only digits, the separators, signs, percent, exponent, spaces and `R$`.
    let body = t.replacen("R$", "$", 1);
    if !body.chars().any(|c| c.is_ascii_digit())
        || !body.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '+' | '-' | '%' | '$' | '(' | ')' | 'e' | 'E' | ' '))
    {
        return None;
    }
    let swapped: String = body
        .chars()
        .map(|c| match c {
            '.' => ',',
            ',' => '.',
            c => c,
        })
        .collect();
    Some(swapped)
}

/// A `TEXT` format code as pt-BR Excel reads it, in the English codes the format parser knows:
/// `A` (ano) is the year, `,` the decimal separator and `.` the thousands separator in numbers,
/// `Geral` the General format. Quoted text, `\`-escaped characters and `[...]` tags stay as they are;
/// `AM/PM` and `A/P` stay AM/PM markers.
pub fn text_code_to_invariant(code: &str) -> String {
    let cs: Vec<char> = code.chars().collect();
    let is_date = date_code(&cs);
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '"' => {
                out.push(c);
                i += 1;
                while i < cs.len() {
                    out.push(cs[i]);
                    i += 1;
                    if cs[i - 1] == '"' {
                        break;
                    }
                }
                continue;
            }
            '\\' => {
                out.push(c);
                if let Some(n) = cs.get(i + 1) {
                    out.push(*n);
                }
                i += 2;
                continue;
            }
            '[' => {
                while i < cs.len() {
                    out.push(cs[i]);
                    i += 1;
                    if cs[i - 1] == ']' {
                        break;
                    }
                }
                continue;
            }
            'a' | 'A' => {
                let rest: String = cs[i..].iter().take(5).collect::<String>().to_ascii_uppercase();
                if rest.starts_with("AM/PM") {
                    out.push_str(&cs[i..i + 5].iter().collect::<String>());
                    i += 5;
                    continue;
                }
                if rest.starts_with("A/P") {
                    out.push_str(&cs[i..i + 3].iter().collect::<String>());
                    i += 3;
                    continue;
                }
                out.push(if c == 'A' { 'Y' } else { 'y' });
            }
            'g' | 'G' if cs[i..].iter().take(5).collect::<String>().eq_ignore_ascii_case("geral") => {
                out.push_str("General");
                i += 5;
                continue;
            }
            // Numbers: comma decimal, dot thousands. Dates keep `.` as text and read `,` after
            // seconds as the decimal point (`ss,00`).
            ',' if !is_date || prev_is_seconds(&out) => out.push('.'),
            '.' if !is_date => out.push(','),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Whether the code (outside quotes, escapes and tags) has date or time letters.
fn date_code(cs: &[char]) -> bool {
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '"' => {
                i += 1;
                while i < cs.len() && cs[i] != '"' {
                    i += 1;
                }
            }
            '\\' => i += 1,
            '[' => {
                while i < cs.len() && cs[i] != ']' {
                    i += 1;
                }
            }
            'd' | 'D' | 'm' | 'M' | 'a' | 'A' | 'y' | 'Y' | 'h' | 'H' | 's' | 'S' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

fn prev_is_seconds(out: &str) -> bool {
    out.trim_end_matches('0').ends_with(['s', 'S'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_codes() {
        assert_eq!(text_code_to_invariant("MMM/AA"), "MMM/YY");
        assert_eq!(text_code_to_invariant("dd/mm/aaaa"), "dd/mm/yyyy");
        assert_eq!(text_code_to_invariant("#.##0,00"), "#,##0.00");
        assert_eq!(text_code_to_invariant("hh:mm AM/PM"), "hh:mm AM/PM");
        assert_eq!(text_code_to_invariant("\"Ano\" aaaa"), "\"Ano\" yyyy");
        assert_eq!(text_code_to_invariant("[$-416]mmmm"), "[$-416]mmmm");
        assert_eq!(text_code_to_invariant("hh:mm:ss,00"), "hh:mm:ss.00");
        assert_eq!(text_code_to_invariant("Geral"), "General");
    }

    #[test]
    fn typed() {
        assert_eq!(typed_to_invariant("1.234,56").as_deref(), Some("1,234.56"));
        assert_eq!(typed_to_invariant("1,5").as_deref(), Some("1.5"));
        assert_eq!(typed_to_invariant("R$ 5,50").as_deref(), Some("$ 5.50"));
        assert_eq!(typed_to_invariant("17/11/2026").as_deref(), Some("11/17/2026"));
        assert_eq!(typed_to_invariant("17/11/2026 10:30").as_deref(), Some("11/17/2026 10:30"));
        assert_eq!(typed_to_invariant("texto"), None);
    }
}
