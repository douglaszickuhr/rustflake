#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokKind {
    Word,
    QuotedIdent,
    Str,
    DollarStr,
    Number,
    Param,
    Comment,
    Semicolon,
    Punct,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokKind,
    pub start: usize,
    pub end: usize,
}

pub fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                TokKind::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Postgres aceita comentários de bloco aninhados.
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                TokKind::Comment
            }
            b'\'' => {
                i = quoted_end(b, i, b'\'', false);
                TokKind::Str
            }
            b'"' => {
                i = quoted_end(b, i, b'"', false);
                TokKind::QuotedIdent
            }
            b'e' | b'E' if b.get(i + 1) == Some(&b'\'') => {
                i = quoted_end(b, i + 1, b'\'', true);
                TokKind::Str
            }
            b'$' => match dollar_tag_end(b, i) {
                Some(open_end) => {
                    let tag = &b[i..open_end];
                    i = b[open_end..]
                        .windows(tag.len())
                        .position(|w| w == tag)
                        .map_or(b.len(), |p| open_end + p + tag.len());
                    TokKind::DollarStr
                }
                None if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    TokKind::Param
                }
                None => {
                    i += 1;
                    TokKind::Punct
                }
            },
            b';' => {
                i += 1;
                TokKind::Semicolon
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                TokKind::Number
            }
            c if is_ident_start(c) => {
                while i < b.len() && is_ident_char(b[i]) {
                    i += 1;
                }
                TokKind::Word
            }
            _ => {
                i += 1;
                TokKind::Punct
            }
        };
        out.push(Token { kind, start, end: i });
    }
    out
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

/// Fim (exclusivo) de um literal que começa em `open` com a aspa `q`.
/// Aspa dobrada é escape; com `backslash`, `\x` também é. Sem fechamento, vai até o fim.
fn quoted_end(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// Se `$` em `i` abre um dollar-quote (`$$` ou `$tag$`), devolve o fim da tag de abertura.
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if j < b.len() && !b[j].is_ascii_digit() {
        while j < b.len() && is_ident_char(b[j]) && b[j] != b'$' {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

/// Literal sem fechamento (string, identificador entre aspas, dollar-quote, comentário)?
/// Re-tokeniza o trecho com um espaço no fim: se o token engolir o espaço, não fechou.
pub fn is_unterminated(src: &str, t: &Token) -> bool {
    let probe = format!("{} ", &src[t.start..t.end]);
    tokenize(&probe).first().is_some_and(|p| p.end == probe.len())
}

/// Nome de um identificador como o Postgres enxerga: sem aspas fica minúsculo.
pub fn ident_text(tok: &str) -> String {
    match tok.strip_prefix('"') {
        Some(rest) => rest.strip_suffix('"').unwrap_or(rest).replace("\"\"", "\""),
        None => tok.to_lowercase(),
    }
}

const RESERVED: &[&str] = &[
    "all", "and", "any", "array", "as", "asc", "both", "case", "cast", "check", "collate", "column",
    "constraint", "create", "default", "desc", "distinct", "do", "else", "end", "except", "false",
    "for", "foreign", "from", "grant", "group", "having", "in", "into", "is", "join", "leading",
    "limit", "not", "null", "offset", "on", "only", "or", "order", "primary", "references", "select",
    "table", "then", "to", "true", "union", "unique", "user", "using", "when", "where", "window", "with",
];

/// Cita o identificador só quando o Postgres exigiria.
pub fn quote_ident(name: &str) -> String {
    let plain = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'$')
        && !RESERVED.contains(&name);
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

#[cfg(test)]
mod tests {
    use super::TokKind::*;
    use super::*;

    fn kinds(src: &str) -> Vec<(TokKind, &str)> {
        tokenize(src).into_iter().map(|t| (t.kind, &src[t.start..t.end])).collect()
    }

    #[test]
    fn words_numbers_punct() {
        assert_eq!(
            kinds("select a1, 3.5 from t;"),
            vec![(Word, "select"), (Word, "a1"), (Punct, ","), (Number, "3.5"), (Word, "from"), (Word, "t"), (Semicolon, ";")]
        );
    }

    #[test]
    fn strings_hide_semicolons() {
        assert_eq!(kinds("'a;''b'"), vec![(Str, "'a;''b'")]);
    }

    #[test]
    fn escape_strings() {
        assert_eq!(kinds(r"E'it\'s;'"), vec![(Str, r"E'it\'s;'")]);
    }

    #[test]
    fn quoted_ident() {
        assert_eq!(kinds(r#""we""ird;""#), vec![(QuotedIdent, r#""we""ird;""#)]);
    }

    #[test]
    fn dollar_quoting() {
        assert_eq!(
            kinds("$$a;b$$ $fn$ x $$ ; $fn$"),
            vec![(DollarStr, "$$a;b$$"), (DollarStr, "$fn$ x $$ ; $fn$")]
        );
    }

    #[test]
    fn params_are_not_dollar_strings() {
        assert_eq!(kinds("$1 = $22"), vec![(Param, "$1"), (Punct, "="), (Param, "$22")]);
    }

    #[test]
    fn comments() {
        assert_eq!(
            kinds("-- a;b\nx /* a /* b; */ c */"),
            vec![(Comment, "-- a;b"), (Word, "x"), (Comment, "/* a /* b; */ c */")]
        );
    }

    #[test]
    fn unterminated_runs_to_end() {
        assert_eq!(kinds("select 'abc"), vec![(Word, "select"), (Str, "'abc")]);
        assert_eq!(kinds("$$ abc"), vec![(DollarStr, "$$ abc")]);
        assert_eq!(kinds("/* abc"), vec![(Comment, "/* abc")]);
    }

    #[test]
    fn non_ascii_identifiers() {
        assert_eq!(kinds("select ação"), vec![(Word, "select"), (Word, "ação")]);
    }

    #[test]
    fn detects_unterminated_literals() {
        let open = |src: &str| is_unterminated(src, &tokenize(src)[0]);
        assert!(open("'abc") && open("'it''") && open("\"Ord") && open("$$ x") && open("/* x") && open("-- x"));
        assert!(!open("'abc'") && !open("\"Ord\"") && !open("$$ x $$") && !open("/* x */"));
    }

    #[test]
    fn ident_text_folds_and_unquotes() {
        assert_eq!(ident_text("Users"), "users");
        assert_eq!(ident_text(r#""My""T""#), r#"My"T"#);
    }

    #[test]
    fn quote_ident_only_when_needed() {
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("Order Items"), r#""Order Items""#);
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
        assert_eq!(quote_ident("select"), r#""select""#);
        assert_eq!(quote_ident("1abc"), r#""1abc""#);
    }
}
