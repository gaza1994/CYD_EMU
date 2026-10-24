//! The TOML subset board profiles are written in: `[table]` / `[a.b]` headers, `key = value` with
//! decimal or `0x` integers (`_` separators), `true`/`false`, basic `"strings"` (`\"`, `\\`,
//! `\n` escapes) and arrays of those, which may span lines; `#` comments. Inline tables, dates,
//! floats, literal and multi-line strings and arrays of tables are refused with the line number.
//! Keys come back flattened (`touch.raw_x`), in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value { Int(i64), Bool(bool), Str(String), Array(Vec<Value>) }

impl Value {
    pub fn as_int(&self) -> Option<i64> { if let Value::Int(v) = self { Some(*v) } else { None } }
    pub fn as_bool(&self) -> Option<bool> { if let Value::Bool(v) = self { Some(*v) } else { None } }
    pub fn as_str(&self) -> Option<&str> { if let Value::Str(v) = self { Some(v) } else { None } }
    pub fn as_array(&self) -> Option<&[Value]> { if let Value::Array(v) = self { Some(v) } else { None } }
}

pub type Table = Vec<(String, Value)>;

pub fn parse(text: &str) -> Result<Table, String> {
    let mut out: Table = Vec::new();
    let mut prefix = String::new();
    let mut lines = text.lines().enumerate();
    while let Some((n, raw)) = lines.next() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() { continue; }
        let at = |e: String| format!("line {}: {e}", n + 1);
        if let Some(h) = line.strip_prefix('[') {
            let h = h.strip_suffix(']').ok_or_else(|| at("unclosed table header".into()))?.trim();
            if h.starts_with('[') || h.is_empty() || !h.split('.').all(bare_key) { return Err(at(format!("unsupported table header [{h}]"))); }
            prefix = format!("{h}.");
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| at("expected key = value".into()))?;
        let k = k.trim();
        if !bare_key(k) { return Err(at(format!("unsupported key '{k}'"))); }
        let mut v = v.trim().to_string();
        // an array may continue over the following lines until its brackets balance
        while depth(&v) > 0 {
            let (_, more) = lines.next().ok_or_else(|| at("unclosed array".into()))?;
            v.push(' '); v.push_str(strip_comment(more).trim());
        }
        let key = format!("{prefix}{k}");
        if out.iter().any(|(e, _)| *e == key) { return Err(at(format!("duplicate key {key}"))); }
        let (value, rest) = value(&v).map_err(at)?;
        if !rest.trim().is_empty() { return Err(at(format!("unexpected '{}'", rest.trim()))); }
        out.push((key, value));
    }
    Ok(out)
}

fn bare_key(k: &str) -> bool { !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') }

/// The line up to a `#` that is not inside a string.
fn strip_comment(line: &str) -> &str {
    let (mut quoted, mut escaped) = (false, false);
    for (i, c) in line.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Open brackets outside strings.
fn depth(s: &str) -> i32 {
    let (mut d, mut quoted, mut escaped) = (0, false, false);
    for c in s.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '[' if !quoted => d += 1,
            ']' if !quoted => d -= 1,
            _ => {}
        }
    }
    d
}

fn value(s: &str) -> Result<(Value, &str), String> {
    let s = s.trim_start();
    if let Some(mut rest) = s.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            rest = rest.trim_start();
            if let Some(r) = rest.strip_prefix(']') { return Ok((Value::Array(items), r)); }
            let (v, r) = value(rest)?;
            items.push(v);
            rest = r.trim_start();
            if let Some(r) = rest.strip_prefix(',') { rest = r; } else if !rest.starts_with(']') { return Err("expected , or ] in array".into()); }
        }
    }
    if let Some(body) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Ok((Value::Str(out), &body[i + 1..])),
                '\\' => match chars.next() {
                    Some((_, '"')) => out.push('"'), Some((_, '\\')) => out.push('\\'), Some((_, 'n')) => out.push('\n'), Some((_, 't')) => out.push('\t'),
                    _ => return Err("unsupported string escape".into()),
                },
                _ => out.push(c),
            }
        }
        return Err("unclosed string".into());
    }
    let end = s.find(|c: char| c == ',' || c == ']' || c.is_whitespace()).unwrap_or(s.len());
    let (word, rest) = s.split_at(end);
    let v = match word {
        "true" => Value::Bool(true), "false" => Value::Bool(false),
        _ => {
            let w = word.replace('_', "");
            let (neg, w) = match w.strip_prefix('-') { Some(w) => (true, w.to_string()), None => (false, w.strip_prefix('+').unwrap_or(&w).to_string()) };
            let v = match w.strip_prefix("0x") { Some(h) => i64::from_str_radix(h, 16), None => w.parse() }.map_err(|_| format!("unsupported value '{word}'"))?;
            Value::Int(if neg { -v } else { v })
        }
    };
    Ok((v, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_scalars_and_arrays_flatten_in_order() {
        let t = parse("name = \"cyd # two\" # comment\n\n[display]\nwidth = 0x140\nflip_y = true\n[touch.xpt2046]\nraw_x = [208, 3_823]\nsources = [\n  \"a\", # one\n  \"b\\\"c\",\n]\nneg = -5\n").unwrap();
        let keys: Vec<&str> = t.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["name", "display.width", "display.flip_y", "touch.xpt2046.raw_x", "touch.xpt2046.sources", "touch.xpt2046.neg"]);
        assert_eq!(t[0].1.as_str(), Some("cyd # two"));
        assert_eq!((t[1].1.as_int(), t[2].1.as_bool(), t[5].1.as_int()), (Some(320), Some(true), Some(-5)));
        assert_eq!(t[3].1, Value::Array(vec![Value::Int(208), Value::Int(3823)]));
        assert_eq!(t[4].1.as_array().unwrap()[1].as_str(), Some("b\"c"));
    }

    #[test]
    fn unsupported_syntax_names_the_line() {
        for (text, line) in [("a = 1\nb = 1.5", 2), ("[[x]]", 1), ("a = { b = 1 }", 1), ("a = 1\na = 2", 2), ("a = [1, 2", 1), ("a = \"x", 1), ("x", 1), ("a = 1 2", 1)] {
            let e = parse(text).unwrap_err();
            assert!(e.starts_with(&format!("line {line}:")), "{text:?}: {e}");
        }
    }
}
