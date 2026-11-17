//! Localized aliases accepted at user-entry boundaries; file syntax stays English.

use crate::{Expr, ParseError, parser};

/// Formula-entry syntax selected by the interface language. Other interface languages
/// currently use the canonical English function table until a translation table is supplied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormulaLocale {
    #[default]
    En,
    Es,
    /// Brazilian Portuguese, as pt-BR Excel: `SOMASE(A:A;B1;C:C)`, decimal comma, `VERDADEIRO`.
    PtBr,
}

const SPANISH: &[(&str, &str)] = &[
    ("SUM", "SUMA"),
    ("AVERAGE", "PROMEDIO"),
    ("IF", "SI"),
    ("COUNT", "CONTAR"),
    ("SUMIF", "SUMAR.SI"),
    ("COUNTIF", "CONTAR.SI"),
    ("VLOOKUP", "BUSCARV"),
    ("TODAY", "HOY"),
    // Financial block (#1986): the loan-schedule and investment staples.
    ("NPV", "VNA"),
    ("IRR", "TIR"),
    ("PMT", "PAGO"),
    ("PV", "VA"),
    ("FV", "VF"),
    ("TRUE", "VERDADERO"),
    ("FALSE", "FALSO"),
];

/// pt-BR Excel's names for the functions people type most; the others keep their English names.
const PORTUGUESE: &[(&str, &str)] = &[
    ("SUM", "SOMA"),
    ("SUMIF", "SOMASE"),
    ("SUMIFS", "SOMASES"),
    ("SUMPRODUCT", "SOMARPRODUTO"),
    ("SUMSQ", "SOMAQUAD"),
    ("AVERAGE", "MÉDIA"),
    ("AVERAGEA", "MÉDIAA"),
    ("AVERAGEIF", "MÉDIASE"),
    ("AVERAGEIFS", "MÉDIASES"),
    ("COUNT", "CONT.NÚM"),
    ("COUNTA", "CONT.VALORES"),
    ("COUNTBLANK", "CONTAR.VAZIO"),
    ("COUNTIF", "CONT.SE"),
    ("COUNTIFS", "CONT.SES"),
    ("MAX", "MÁXIMO"),
    ("MIN", "MÍNIMO"),
    ("MAXIFS", "MÁXIMOSES"),
    ("MINIFS", "MÍNIMOSES"),
    ("MEDIAN", "MED"),
    ("MODE", "MODO"),
    ("LARGE", "MAIOR"),
    ("SMALL", "MENOR"),
    ("RANK", "ORDEM"),
    ("RANK.EQ", "ORDEM.EQ"),
    ("PERCENTILE", "PERCENTIL"),
    ("STDEV", "DESVPAD"),
    ("STDEV.S", "DESVPAD.A"),
    ("STDEV.P", "DESVPAD.P"),
    ("IF", "SE"),
    ("IFS", "SES"),
    ("IFERROR", "SEERRO"),
    ("IFNA", "SENÃODISP"),
    ("AND", "E"),
    ("OR", "OU"),
    ("NOT", "NÃO"),
    ("XOR", "OUEXCL"),
    ("SWITCH", "PARÂMETRO"),
    ("VLOOKUP", "PROCV"),
    ("HLOOKUP", "PROCH"),
    ("LOOKUP", "PROC"),
    ("XLOOKUP", "PROCX"),
    ("INDEX", "ÍNDICE"),
    ("MATCH", "CORRESP"),
    ("XMATCH", "CORRESPX"),
    ("OFFSET", "DESLOC"),
    ("INDIRECT", "INDIRETO"),
    ("CHOOSE", "ESCOLHER"),
    ("ROW", "LIN"),
    ("ROWS", "LINS"),
    ("COLUMN", "COL"),
    ("COLUMNS", "COLS"),
    ("ADDRESS", "ENDEREÇO"),
    ("TRANSPOSE", "TRANSPOR"),
    ("UNIQUE", "ÚNICO"),
    ("FILTER", "FILTRO"),
    ("SORT", "CLASSIFICAR"),
    ("SORTBY", "CLASSIFICARPOR"),
    ("SEQUENCE", "SEQUÊNCIA"),
    ("ROUND", "ARRED"),
    ("ROUNDUP", "ARREDONDAR.PARA.CIMA"),
    ("ROUNDDOWN", "ARREDONDAR.PARA.BAIXO"),
    ("MROUND", "MARRED"),
    ("TRUNC", "TRUNCAR"),
    ("CEILING", "TETO"),
    ("FLOOR", "ARREDMULTB"),
    ("CEILING.MATH", "TETO.MAT"),
    ("FLOOR.MATH", "ARREDMULTB.MAT"),
    ("POWER", "POTÊNCIA"),
    ("SQRT", "RAIZ"),
    ("PRODUCT", "MULT"),
    ("SIGN", "SINAL"),
    ("EVEN", "PAR"),
    ("ODD", "ÍMPAR"),
    ("ISEVEN", "ÉPAR"),
    ("ISODD", "ÉIMPAR"),
    ("RAND", "ALEATÓRIO"),
    ("RANDBETWEEN", "ALEATÓRIOENTRE"),
    ("TODAY", "HOJE"),
    ("NOW", "AGORA"),
    ("DATE", "DATA"),
    ("TIME", "TEMPO"),
    ("YEAR", "ANO"),
    ("MONTH", "MÊS"),
    ("DAY", "DIA"),
    ("HOUR", "HORA"),
    ("MINUTE", "MINUTO"),
    ("SECOND", "SEGUNDO"),
    ("WEEKDAY", "DIA.DA.SEMANA"),
    ("WEEKNUM", "NÚMSEMANA"),
    ("EOMONTH", "FIMMÊS"),
    ("EDATE", "DATAM"),
    ("DATEDIF", "DATADIF"),
    ("DAYS", "DIAS"),
    ("DATEVALUE", "DATA.VALOR"),
    ("NETWORKDAYS", "DIATRABALHOTOTAL"),
    ("NETWORKDAYS.INTL", "DIATRABALHOTOTAL.INTL"),
    ("WORKDAY", "DIATRABALHO"),
    ("WORKDAY.INTL", "DIATRABALHO.INTL"),
    ("YEARFRAC", "FRAÇÃOANO"),
    ("TEXT", "TEXTO"),
    ("CONCATENATE", "CONCATENAR"),
    ("TEXTJOIN", "UNIRTEXTO"),
    ("LEFT", "ESQUERDA"),
    ("RIGHT", "DIREITA"),
    ("MID", "EXT.TEXTO"),
    ("LEN", "NÚM.CARACT"),
    ("UPPER", "MAIÚSCULA"),
    ("LOWER", "MINÚSCULA"),
    ("PROPER", "PRI.MAIÚSCULA"),
    ("TRIM", "ARRUMAR"),
    ("CLEAN", "TIRAR"),
    ("SUBSTITUTE", "SUBSTITUIR"),
    ("REPLACE", "MUDAR"),
    ("FIND", "PROCURAR"),
    ("SEARCH", "LOCALIZAR"),
    ("VALUE", "VALOR"),
    ("NUMBERVALUE", "VALORNUMÉRICO"),
    ("EXACT", "EXATO"),
    ("CHAR", "CARACT"),
    ("CODE", "CÓDIGO"),
    ("FIXED", "DEF.NÚM.DEC"),
    ("DOLLAR", "MOEDA"),
    ("TEXTBEFORE", "TEXTOANTES"),
    ("TEXTAFTER", "TEXTODEPOIS"),
    ("TEXTSPLIT", "DIVIDIRTEXTO"),
    ("ISBLANK", "ÉCÉL.VAZIA"),
    ("ISERROR", "ÉERROS"),
    ("ISERR", "ÉERRO"),
    ("ISNA", "É.NÃO.DISP"),
    ("ISNUMBER", "ÉNÚM"),
    ("ISTEXT", "ÉTEXTO"),
    ("ISLOGICAL", "ÉLÓGICO"),
    ("ISFORMULA", "ÉFÓRMULA"),
    ("FORMULATEXT", "FÓRMULATEXTO"),
    ("NA", "NÃO.DISP"),
    ("AGGREGATE", "AGREGAR"),
    ("HYPERLINK", "HIPERLINK"),
    ("CONVERT", "CONVERTER"),
    ("VSTACK", "EMPILHARV"),
    ("HSTACK", "EMPILHARH"),
    ("CHOOSECOLS", "ESCOLHERCOLS"),
    ("CHOOSEROWS", "ESCOLHERLINS"),
    ("NPV", "VPL"),
    ("IRR", "TIR"),
    ("PMT", "PGTO"),
    ("PV", "VP"),
    ("FV", "VF"),
    ("RATE", "TAXA"),
    ("TRUE", "VERDADEIRO"),
    ("FALSE", "FALSO"),
];

impl FormulaLocale {
    pub fn list_separator(self) -> char {
        if self == Self::En { ',' } else { ';' }
    }
    pub fn decimal_separator(self) -> char {
        if self == Self::En { '.' } else { ',' }
    }
    pub fn array_column_separator(self) -> char {
        if self == Self::En { ',' } else { '\\' }
    }
    /// The function names this syntax translates, (English, localized).
    fn table(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::En => &[],
            Self::Es => SPANISH,
            Self::PtBr => PORTUGUESE,
        }
    }
    pub fn function_name(self, canonical: &str) -> &str {
        self.table().iter().find(|(en, _)| en.eq_ignore_ascii_case(canonical)).map(|(_, local)| *local).unwrap_or(canonical)
    }
    /// The English name of a localized one. Case and accents don't matter (`media` reads as
    /// `MÉDIA`, so AVERAGE).
    pub fn canonical_name(self, localized: &str) -> &str {
        let wanted = fold(localized);
        self.table().iter().find(|(_, local)| fold(local) == wanted).map(|(en, _)| *en).unwrap_or(localized)
    }
    /// The words for TRUE and FALSE.
    fn booleans(self) -> (&'static str, &'static str) {
        match self {
            Self::En => ("TRUE", "FALSE"),
            Self::Es => ("VERDADERO", "FALSO"),
            Self::PtBr => ("VERDADEIRO", "FALSO"),
        }
    }
}

/// Upper case without the accents Portuguese and Spanish names carry.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_uppercase)
        .map(|c| match c {
            'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => 'A',
            'É' | 'È' | 'Ê' | 'Ë' => 'E',
            'Í' | 'Ì' | 'Î' | 'Ï' => 'I',
            'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'O',
            'Ú' | 'Ù' | 'Û' | 'Ü' => 'U',
            'Ç' => 'C',
            'Ñ' => 'N',
            c => c,
        })
        .collect()
}

/// Parses UI input using the selected function table and separators. Existing names and
/// LET/LAMBDA bindings take precedence over aliases. File and API syntax uses [`crate::parse`].
pub fn parse_input(src: &str, locale: FormulaLocale, known_name: impl Fn(&str) -> bool) -> Result<Expr, ParseError> {
    let mut expr = parser::parse_syntax(src, locale)?;
    if locale != FormulaLocale::En {
        normalize(&mut expr, locale, &known_name, &mut Bindings::default(), 0, false)?;
    }
    Ok(expr)
}

/// Prints canonical expressions for editing. Name collisions retain the canonical spelling
/// so displaying then committing a formula never changes a defined-name reference.
pub fn print_input(expr: &Expr, locale: FormulaLocale, known_name: impl Fn(&str) -> bool) -> Result<String, ParseError> {
    // Check before cloning/printing. A flat operator chain such as A1+A2+… nests on its left
    // without nesting in the text: cloning, printing and `normalize` follow that left spine in
    // a loop, so it does not count (Excel accepts 4,096 operands, #916). Other nesting does.
    let mut pending = vec![(expr, 0usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > parser::MAX_DEPTH {
            return Err(ParseError { msg: "formula is nested too deeply".into(), pos: 0 });
        }
        match node {
            Expr::Unary(_, child) | Expr::Paren(child) => pending.push((child, depth + 1)),
            Expr::Binary(_, left, right) => {
                pending.push((left, depth));
                pending.push((right, depth + 1));
            }
            Expr::Call(_, args) => pending.extend(args.iter().map(|e| (e, depth + 1))),
            Expr::Invoke(callee, args) => {
                pending.push((callee, depth + 1));
                pending.extend(args.iter().map(|e| (e, depth + 1)));
            }
            Expr::Array(rows) => pending.extend(rows.iter().flatten().map(|e| (e, depth + 1))),
            _ => {}
        }
    }
    if locale == FormulaLocale::En {
        return Ok(crate::print(expr));
    }
    let mut expr = expr.clone();
    normalize(&mut expr, locale, &known_name, &mut Bindings::default(), 0, true)?;
    let canonical = crate::print(&expr);
    let tokens = crate::lexer::tokenize(&canonical).map_err(|e| ParseError { msg: e.msg, pos: e.pos })?;
    let mut out = String::new();
    let mut array_depth = 0usize;
    for token in tokens {
        use crate::lexer::Tok;
        let text = canonical.get(token.start..token.end).unwrap_or("");
        match token.tok {
            Tok::LBrace => {
                array_depth += 1;
                out.push_str(text);
            }
            Tok::RBrace => {
                array_depth = array_depth.saturating_sub(1);
                out.push_str(text);
            }
            Tok::Number(_) => out.push_str(&text.replace('.', &locale.decimal_separator().to_string())),
            Tok::Comma => out.push(if array_depth > 0 { locale.array_column_separator() } else { locale.list_separator() }),
            _ => out.push_str(text),
        }
    }
    Ok(out)
}

pub(crate) fn boolean_alias(locale: FormulaLocale, name: &str) -> Option<bool> {
    let (yes, no) = locale.booleans();
    if name.eq_ignore_ascii_case(yes) {
        Some(true)
    } else if name.eq_ignore_ascii_case(no) {
        Some(false)
    } else {
        None
    }
}

/// The LET/LAMBDA names in scope, with how many times each is bound, so a lookup is one probe
/// instead of a scan of every binding for each call.
#[derive(Default)]
struct Bindings {
    stack: Vec<String>,
    counts: std::collections::HashMap<String, usize>,
}

impl Bindings {
    fn len(&self) -> usize {
        self.stack.len()
    }

    fn push(&mut self, name: &str) {
        let key = crate::fold_name(name);
        *self.counts.entry(key.clone()).or_default() += 1;
        self.stack.push(key);
    }

    fn truncate(&mut self, len: usize) {
        while self.stack.len() > len {
            let Some(key) = self.stack.pop() else { break };
            if let Some(n) = self.counts.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    self.counts.remove(&key);
                }
            }
        }
    }

    fn contains(&self, name: &str) -> bool {
        self.counts.contains_key(&crate::fold_name(name))
    }
}

fn normalize(expr: &mut Expr, locale: FormulaLocale, known_name: &impl Fn(&str) -> bool, bindings: &mut Bindings, depth: usize, display: bool) -> Result<(), ParseError> {
    if depth > parser::MAX_DEPTH {
        return Err(ParseError { msg: "formula is nested too deeply".into(), pos: 0 });
    }
    let bound = |name: &str, bindings: &Bindings| bindings.contains(name) || known_name(name);
    match expr {
        Expr::Bool(value) if display => {
            let alias = if *value { locale.booleans().0 } else { locale.booleans().1 };
            if !bound(alias, bindings) {
                *expr = Expr::Name(alias.into());
            }
        }
        Expr::Name(name) if !display && !bound(name, bindings) => {
            if let Some(value) = boolean_alias(locale, name) {
                *expr = Expr::Bool(value);
            }
        }
        Expr::Call(name, args) if name == "LET" && args.len() >= 3 && !args.len().is_multiple_of(2) => {
            let saved = bindings.len();
            if let Some((body, pairs)) = args.split_last_mut() {
                for pair in pairs.chunks_mut(2) {
                    if let [declaration, value] = pair {
                        // The new name is not in scope in its own value expression.
                        normalize(value, locale, known_name, bindings, depth + 1, display)?;
                        if let Expr::Name(name) = declaration {
                            bindings.push(name);
                        }
                    }
                }
                normalize(body, locale, known_name, bindings, depth + 1, display)?;
            }
            bindings.truncate(saved);
        }
        Expr::Call(name, args) if name == "LAMBDA" => {
            let saved = bindings.len();
            if let Some((body, params)) = args.split_last_mut() {
                for param in params {
                    if let Expr::Name(name) = param {
                        bindings.push(name);
                    }
                }
                normalize(body, locale, known_name, bindings, depth + 1, display)?;
            }
            bindings.truncate(saved);
        }
        Expr::Call(name, args) => {
            let alias = if display { locale.function_name(name) } else { locale.canonical_name(name) };
            if !bound(name, bindings) && (!display || !bound(alias, bindings)) {
                *name = alias.to_string();
            }
            for arg in args {
                normalize(arg, locale, known_name, bindings, depth + 1, display)?;
            }
        }
        Expr::Invoke(callee, args) => {
            normalize(callee, locale, known_name, bindings, depth + 1, display)?;
            for arg in args {
                normalize(arg, locale, known_name, bindings, depth + 1, display)?;
            }
        }
        Expr::Unary(_, child) | Expr::Paren(child) => normalize(child, locale, known_name, bindings, depth + 1, display)?,
        Expr::Binary(..) => {
            // Follow the left spine of a flat chain (A1+A2+…) in a loop: it is not nesting.
            // Each call leaves `bindings` as it found them, so the order does not matter.
            let mut node = expr;
            while let Expr::Binary(_, left, right) = node {
                normalize(right, locale, known_name, bindings, depth + 1, display)?;
                node = left;
            }
            normalize(node, locale, known_name, bindings, depth, display)?;
        }
        Expr::Array(rows) if display => {
            for row in rows {
                for child in row {
                    normalize(child, locale, known_name, bindings, depth + 1, display)?;
                }
            }
        }
        // Array elements have already been parsed as literals. Sheet/structured references
        // contain user-defined labels and must not be translated.
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod pt_br_tests {
    use super::*;

    fn read(s: &str) -> String {
        crate::print(&parse_input(s, FormulaLocale::PtBr, |_| false).expect("parses"))
    }

    fn show(s: &str) -> String {
        print_input(&crate::parse(s).expect("parses"), FormulaLocale::PtBr, |_| false).expect("prints")
    }

    #[test]
    fn typed_portuguese_reads_as_english() {
        assert_eq!(read("SOMASE('525_AA_ITJ2-JSU '!BI:BI;CRONOGRAMA!G$3;'525_AA_ITJ2-JSU '!BL:BL)"), "SUMIF('525_AA_ITJ2-JSU '!BI:BI,CRONOGRAMA!G$3,'525_AA_ITJ2-JSU '!BL:BL)");
        assert_eq!(read("MÉDIA(1,5;2)"), "AVERAGE(1.5,2)");
        assert_eq!(read("media(A1:A3)"), "AVERAGE(A1:A3)");
        assert_eq!(read("SE(E(A1>0;VERDADEIRO);PROCV(A1;B:C;2;FALSO);\"não\")"), "IF(AND(A1>0,TRUE),VLOOKUP(A1,B:C,2,FALSE),\"não\")");
        assert_eq!(read("TEXTO(A1;\"MMM/AA\")"), "TEXT(A1,\"MMM/AA\")");
        assert_eq!(read("CONT.NÚM(A:A)+cont.num(B:B)"), "COUNT(A:A)+COUNT(B:B)");
        // Functions without a Portuguese name keep the English one.
        assert_eq!(read("LET(x;1;x+1)"), "LET(x,1,x+1)");
    }

    #[test]
    fn english_shows_in_portuguese() {
        assert_eq!(show("SUMIF(A:A,B1,C:C)"), "SOMASE(A:A;B1;C:C)");
        assert_eq!(show("IFERROR(A1/B1,0.5)"), "SEERRO(A1/B1;0,5)");
        assert_eq!(show("IF(A1,TRUE,FALSE)"), "SE(A1;VERDADEIRO;FALSO)");
    }
}
