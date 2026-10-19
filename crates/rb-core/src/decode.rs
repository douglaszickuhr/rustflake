use tokio_postgres::types::{Kind, Type};

/// Full value text, in the format Postgres accepts back as input.
pub fn cell_text(ty: &Type, raw: &[u8]) -> String {
    decode(ty, raw).unwrap_or_else(|| format!("<{}: {} bytes>", ty.name(), raw.len()))
}

/// Text for the grid: one line, at most `max_chars` characters (+ `…`).
/// For large text types and bytea, decodes only the beginning.
pub fn display_text(ty: &Type, raw: &[u8], max_chars: usize) -> String {
    let cap = max_chars * 4 + 8;
    let text = if *ty == Type::BYTEA && raw.len() > max_chars {
        format!("\\x{}", hex(&raw[..max_chars / 2 + 1]))
    } else if raw.len() > cap && is_textual(ty) {
        let body = if *ty == Type::JSONB { &raw[1..] } else { raw };
        String::from_utf8_lossy(&body[..cap.min(body.len())]).into_owned()
    } else {
        cell_text(ty, raw)
    };
    one_line(&text, max_chars)
}

/// Replaces control characters with spaces and cuts at `max_chars` with `…`.
pub fn one_line(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4) + 3);
    for (n, ch) in s.chars().enumerate() {
        if n == max_chars {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

/// Full value for detailed reading: indented JSON (key order kept),
/// everything else as is; cuts at `max_chars` with `…`.
pub fn detail_text(s: &str, max_chars: usize) -> String {
    let t = s.trim_start();
    let pretty = (t.starts_with('{') || t.starts_with('[')).then(|| serde_json::from_str::<serde_json::Value>(t).ok()).flatten().and_then(|v| serde_json::to_string_pretty(&v).ok());
    let s = pretty.as_deref().unwrap_or(s);
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn is_textual(ty: &Type) -> bool {
    matches!(*ty, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::JSON | Type::JSONB | Type::XML) || matches!(ty.kind(), Kind::Enum(_))
}

fn decode(ty: &Type, r: &[u8]) -> Option<String> {
    match ty.kind() {
        Kind::Enum(_) => return Some(utf8(r)),
        Kind::Domain(base) => return decode(base, r),
        Kind::Array(elem) => return array(elem, r),
        _ => {}
    }
    Some(match *ty {
        Type::BOOL => (if *r.first()? != 0 { "true" } else { "false" }).to_string(),
        Type::INT2 => i16::from_be_bytes(be(r, 0)?).to_string(),
        Type::INT4 => i32::from_be_bytes(be(r, 0)?).to_string(),
        Type::INT8 => i64::from_be_bytes(be(r, 0)?).to_string(),
        Type::OID => u32::from_be_bytes(be(r, 0)?).to_string(),
        Type::FLOAT4 => float(f32::from_be_bytes(be(r, 0)?)),
        Type::FLOAT8 => float(f64::from_be_bytes(be(r, 0)?)),
        Type::NUMERIC => numeric(r)?,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::JSON | Type::XML | Type::UNKNOWN => utf8(r),
        Type::JSONB => utf8(r.get(1..)?),
        Type::CHAR => (*r.first()? as char).to_string(),
        Type::UUID => uuid(r)?,
        Type::BYTEA => format!("\\x{}", hex(r)),
        Type::DATE => date(i32::from_be_bytes(be(r, 0)?))?,
        Type::TIME => time(i64::from_be_bytes(be(r, 0)?)),
        Type::TIMESTAMP => timestamp(i64::from_be_bytes(be(r, 0)?))?,
        Type::TIMESTAMPTZ => {
            let t = timestamp(i64::from_be_bytes(be(r, 0)?))?;
            if t.ends_with("infinity") { t } else { t + "+00" }
        }
        Type::INTERVAL => interval(i64::from_be_bytes(be(r, 0)?), i32::from_be_bytes(be(r, 8)?), i32::from_be_bytes(be(r, 12)?)),
        _ => return None,
    })
}

fn be<const N: usize>(r: &[u8], at: usize) -> Option<[u8; N]> {
    r.get(at..at + N)?.try_into().ok()
}

fn utf8(r: &[u8]) -> String {
    String::from_utf8_lossy(r).into_owned()
}

fn float<F: std::fmt::Display + Into<f64> + Copy>(f: F) -> String {
    let x: f64 = f.into();
    if x.is_infinite() {
        (if x > 0.0 { "Infinity" } else { "-Infinity" }).to_string()
    } else if x.is_nan() {
        "NaN".to_string()
    } else {
        f.to_string()
    }
}

fn hex(r: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(r.len() * 2);
    for b in r {
        s.push(D[(b >> 4) as usize] as char);
        s.push(D[(b & 15) as usize] as char);
    }
    s
}

fn uuid(r: &[u8]) -> Option<String> {
    let h = hex(r.get(..16)?);
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Binary numeric format: ndigits, weight, sign, dscale, base-10000 digits.
fn numeric(r: &[u8]) -> Option<String> {
    let ndigits = i16::from_be_bytes(be(r, 0)?) as i32;
    let weight = i16::from_be_bytes(be(r, 2)?) as i32;
    let sign = u16::from_be_bytes(be(r, 4)?);
    let dscale = u16::from_be_bytes(be(r, 6)?) as usize;
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => {}
    }
    let digit = |k: i32| -> Option<i16> {
        if k < 0 || k >= ndigits {
            return Some(0);
        }
        Some(i16::from_be_bytes(be(r, 8 + 2 * k as usize)?))
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for k in 0..=weight {
            let d = digit(k)?;
            if k == 0 { s += &d.to_string() } else { s += &format!("{d:04}") }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let mut k = weight + 1;
        while frac.len() < dscale {
            frac += &format!("{:04}", digit(k)?);
            k += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s += &frac;
    }
    Some(s)
}

const DAY_US: i64 = 86_400_000_000;

/// Days since Postgres's epoch (2000-01-01). ponytail: beyond chrono's ±262k years → raw bytes shown.
fn ymd(days_since_2000: i64) -> Option<String> {
    let epoch = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)?;
    let d = chrono::Days::new(days_since_2000.unsigned_abs());
    let day = if days_since_2000 < 0 { epoch.checked_sub_days(d) } else { epoch.checked_add_days(d) }?;
    Some(day.format("%Y-%m-%d").to_string())
}

fn date(days: i32) -> Option<String> {
    match days {
        i32::MAX => Some("infinity".into()),
        i32::MIN => Some("-infinity".into()),
        d => ymd(d.into()),
    }
}

fn time(us: i64) -> String {
    let (s, frac) = (us.div_euclid(1_000_000), us.rem_euclid(1_000_000));
    let mut out = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    if frac != 0 {
        out += format!(".{frac:06}").trim_end_matches('0');
    }
    out
}

fn timestamp(us: i64) -> Option<String> {
    match us {
        i64::MAX => Some("infinity".into()),
        i64::MIN => Some("-infinity".into()),
        us => Some(format!("{} {}", ymd(us.div_euclid(DAY_US))?, time(us.rem_euclid(DAY_US)))),
    }
}

fn interval(us: i64, days: i32, months: i32) -> String {
    let plural = |n: i32, one: &str, many: &str| format!("{n} {}", if n.abs() == 1 { one } else { many });
    let mut parts = Vec::new();
    if months / 12 != 0 {
        parts.push(plural(months / 12, "year", "years"));
    }
    if months % 12 != 0 {
        parts.push(plural(months % 12, "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(days, "day", "days"));
    }
    if us != 0 || parts.is_empty() {
        let sign = if us < 0 { "-" } else { "" };
        parts.push(format!("{sign}{}", time(us.unsigned_abs() as i64)));
    }
    parts.join(" ")
}

fn array(elem: &Type, r: &[u8]) -> Option<String> {
    let ndim = i32::from_be_bytes(be(r, 0)?) as usize;
    if ndim == 0 {
        return Some("{}".into());
    }
    let mut dims = Vec::with_capacity(ndim);
    for d in 0..ndim {
        dims.push(i32::from_be_bytes(be(r, 12 + d * 8)?) as usize);
    }
    let mut pos = 12 + ndim * 8;
    let mut out = String::new();
    array_dim(elem, r, &dims, &mut pos, &mut out)?;
    Some(out)
}

fn array_dim(elem: &Type, r: &[u8], dims: &[usize], pos: &mut usize, out: &mut String) -> Option<()> {
    out.push('{');
    for i in 0..dims[0] {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            array_dim(elem, r, &dims[1..], pos, out)?;
            continue;
        }
        let len = i32::from_be_bytes(be(r, *pos)?);
        *pos += 4;
        if len < 0 {
            out.push_str("NULL");
            continue;
        }
        let v = r.get(*pos..*pos + len as usize)?;
        *pos += len as usize;
        let s = cell_text(elem, v);
        let needs_quotes = s.is_empty() || s.eq_ignore_ascii_case("null") || s.contains(|c: char| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_whitespace());
        if needs_quotes {
            out.push('"');
            for c in s.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out.push('}');
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_text_indents_json_and_caps_size() {
        assert_eq!(detail_text(r#"{"a":[1,2]}"#, 100), "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert_eq!(detail_text("  [true]", 100), "[\n  true\n]");
        assert_eq!(detail_text(r#"{"b":1,"a":2}"#, 100), "{\n  \"b\": 1,\n  \"a\": 2\n}");
        // Text that is not JSON stays as is, even if it starts with a brace.
        assert_eq!(detail_text("{not json", 100), "{not json");
        assert_eq!(detail_text("line 1\nline 2", 100), "line 1\nline 2");
        assert_eq!(detail_text("abcdef", 3), "abc…");
    }

    fn numeric(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(ndigits.to_be_bytes());
        v.extend(weight.to_be_bytes());
        v.extend(sign.to_be_bytes());
        v.extend(dscale.to_be_bytes());
        for d in digits {
            v.extend(d.to_be_bytes());
        }
        v
    }

    #[test]
    fn scalars() {
        assert_eq!(cell_text(&Type::BOOL, &[1]), "true");
        assert_eq!(cell_text(&Type::INT2, &(-7i16).to_be_bytes()), "-7");
        assert_eq!(cell_text(&Type::INT4, &42i32.to_be_bytes()), "42");
        assert_eq!(cell_text(&Type::INT8, &(1i64 << 40).to_be_bytes()), "1099511627776");
        assert_eq!(cell_text(&Type::FLOAT8, &1.5f64.to_be_bytes()), "1.5");
        assert_eq!(cell_text(&Type::FLOAT4, &f32::INFINITY.to_be_bytes()), "Infinity");
        assert_eq!(cell_text(&Type::TEXT, "olá".as_bytes()), "olá");
        assert_eq!(cell_text(&Type::CHAR, b"r"), "r");
        assert_eq!(cell_text(&Type::BYTEA, &[0xde, 0xad]), "\\xdead");
    }

    #[test]
    fn numerics() {
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(2, 0, 0, 2, &[123, 4500])), "123.45");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(1, -1, 0x4000, 3, &[10])), "-0.001");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(1, 1, 0, 0, &[1])), "10000");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(0, 0, 0, 0, &[])), "0");
        assert_eq!(cell_text(&Type::NUMERIC, &numeric(0, 0, 0xC000, 0, &[])), "NaN");
    }

    #[test]
    fn dates_and_times() {
        // 2024-02-29 = 8825 days after 2000-01-01
        assert_eq!(cell_text(&Type::DATE, &8825i32.to_be_bytes()), "2024-02-29");
        assert_eq!(cell_text(&Type::DATE, &(-1i32).to_be_bytes()), "1999-12-31");
        assert_eq!(cell_text(&Type::DATE, &i32::MAX.to_be_bytes()), "infinity");
        let us: i64 = 8825 * 86_400_000_000 + (13 * 3600 + 45 * 60 + 30) * 1_000_000 + 500_000;
        assert_eq!(cell_text(&Type::TIMESTAMP, &us.to_be_bytes()), "2024-02-29 13:45:30.5");
        assert_eq!(cell_text(&Type::TIMESTAMPTZ, &us.to_be_bytes()), "2024-02-29 13:45:30.5+00");
        assert_eq!(cell_text(&Type::TIMESTAMPTZ, &i64::MAX.to_be_bytes()), "infinity");
        assert_eq!(cell_text(&Type::TIME, &(3_600_000_000i64).to_be_bytes()), "01:00:00");
    }

    #[test]
    fn intervals() {
        let mut v = Vec::new();
        v.extend(((4 * 3600 + 5 * 60 + 6) * 1_000_000i64).to_be_bytes());
        v.extend(3i32.to_be_bytes());
        v.extend(14i32.to_be_bytes());
        assert_eq!(cell_text(&Type::INTERVAL, &v), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(cell_text(&Type::INTERVAL, &[0u8; 16]), "00:00:00");
    }

    #[test]
    fn uuid_json_jsonb() {
        let u: Vec<u8> = (0u8..16).collect();
        assert_eq!(cell_text(&Type::UUID, &u), "00010203-0405-0607-0809-0a0b0c0d0e0f");
        assert_eq!(cell_text(&Type::JSON, br#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(cell_text(&Type::JSONB, b"\x01{\"a\":1}"), r#"{"a":1}"#);
    }

    #[test]
    fn arrays() {
        let mut v = Vec::new();
        for x in [1i32, 1, 23, 3, 1] {
            v.extend(x.to_be_bytes()); // ndim, has_null, oid, len, lbound
        }
        v.extend(4i32.to_be_bytes());
        v.extend(1i32.to_be_bytes());
        v.extend((-1i32).to_be_bytes());
        v.extend(4i32.to_be_bytes());
        v.extend(3i32.to_be_bytes());
        assert_eq!(cell_text(&Type::INT4_ARRAY, &v), "{1,NULL,3}");

        let mut t = Vec::new();
        for x in [1i32, 0, 25, 2, 1] {
            t.extend(x.to_be_bytes());
        }
        t.extend(3i32.to_be_bytes());
        t.extend(b"a b");
        t.extend(0i32.to_be_bytes());
        assert_eq!(cell_text(&Type::TEXT_ARRAY, &t), r#"{"a b",""}"#);
    }

    #[test]
    fn enums_domains_and_unknown() {
        let mood = Type::new("mood".into(), 90001, Kind::Enum(vec![]), "public".into());
        assert_eq!(cell_text(&mood, b"sad"), "sad");
        let dom = Type::new("pos".into(), 90002, Kind::Domain(Type::INT4), "public".into());
        assert_eq!(cell_text(&dom, &5i32.to_be_bytes()), "5");
        assert_eq!(cell_text(&Type::POINT, &[0u8; 16]), "<point: 16 bytes>");
        assert_eq!(cell_text(&Type::INT4, &[1, 2]), "<int4: 2 bytes>");
    }

    #[test]
    fn display_is_single_line() {
        assert_eq!(display_text(&Type::TEXT, b"ab\ncd\te", 50), "ab cd e");
    }

    #[test]
    fn display_truncates_huge_text_without_full_decode() {
        let big = vec![b'x'; 5_000_000];
        let s = display_text(&Type::TEXT, &big, 100);
        assert_eq!(s.chars().count(), 101);
        assert!(s.ends_with('…'));
        let jb = [b"\x01".as_slice(), &vec![b'y'; 1_000_000]].concat();
        assert!(display_text(&Type::JSONB, &jb, 10).starts_with("yyyyyyyyyy…"));
        assert_eq!(display_text(&Type::BYTEA, &vec![0xab; 1_000_000], 10), "\\xabababab…");
    }
}
