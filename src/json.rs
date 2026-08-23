//! 极简 JSON 库(零依赖):解析 + 生成
//!
//! 支持标准 JSON 子集:对象 / 数组 / 字符串 / 数字 / bool / null
//! 足够 API、MCP、索引持久化使用

use std::collections::BTreeMap;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }
    pub fn num(n: f64) -> Json {
        Json::Num(n)
    }
    pub fn arr(v: Vec<Json>) -> Json {
        Json::Arr(v)
    }
    pub fn obj() -> Json {
        Json::Obj(BTreeMap::new())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// 构建对象:交替 key/value
    pub fn build(pairs: Vec<(&str, Json)>) -> Json {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v);
        }
        Json::Obj(m)
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_json(self, f, 0)
    }
}

fn write_json(v: &Json, f: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
    match v {
        Json::Null => write!(f, "null"),
        Json::Bool(b) => write!(f, "{}", b),
        Json::Num(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                write!(f, "{}", *n as i64)
            } else {
                write!(f, "{}", n)
            }
        }
        Json::Str(s) => write!(f, "\"{}\"", escape(s)),
        Json::Arr(a) => {
            if a.is_empty() {
                return write!(f, "[]");
            }
            writeln!(f, "[")?;
            for (i, item) in a.iter().enumerate() {
                write!(f, "{:indent$}", "", indent = indent + 2)?;
                write_json(item, f, indent + 2)?;
                if i + 1 < a.len() {
                    write!(f, ",")?;
                }
                writeln!(f)?;
            }
            write!(f, "{:indent$}]", "", indent = indent)
        }
        Json::Obj(m) => {
            if m.is_empty() {
                return write!(f, "{{}}");
            }
            writeln!(f, "{{")?;
            for (i, (k, val)) in m.iter().enumerate() {
                let key = escape(k);
                write!(f, "{:indent$}\"{}\": ", "", key, indent = indent + 2)?;
                write_json(val, f, indent + 2)?;
                if i + 1 < m.len() {
                    write!(f, ",")?;
                }
                writeln!(f)?;
            }
            write!(f, "{:indent$}}}", "", indent = indent)
        }
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ---------------- 解析 ----------------

pub struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser { bytes: s.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("JSON 意外结束".into()),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b't') => {
                self.expect_lit("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.expect_lit("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.expect_lit("null")?;
                Ok(Json::Null)
            }
            Some(_) => self.parse_number(),
        }
    }

    fn expect_lit(&mut self, lit: &str) -> Result<(), String> {
        if self.bytes.len() >= self.pos + lit.len()
            && &self.bytes[self.pos..self.pos + lit.len()] == lit.as_bytes()
        {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(format!("JSON 语法错误: 期望 {}", lit))
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err("JSON 字符串必须以 \" 开头".into());
        }
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(&b) = self.bytes.get(self.pos) else {
                return Err("JSON 字符串未闭合".into());
            };
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(&esc) = self.bytes.get(self.pos) else {
                        return Err("JSON 转义未完成".into());
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            if self.pos + 4 > self.bytes.len() {
                                return Err("JSON \\u 转义过短".into());
                            }
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
                                .map_err(|_| "JSON \\u 非 UTF-8".to_string())?;
                            let code = u32::from_str_radix(hex, 16).map_err(|_| "JSON \\u 非法".to_string())?;
                            self.pos += 4;
                            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                        }
                        other => return Err(format!("JSON 非法转义: \\{}", other as char)),
                    }
                }
                b if b < 0x20 => return Err("JSON 字符串含未转义控制字符".into()),
                _ => {
                    // 手动解码 UTF-8 首个字符(避免 from_utf8 全量验证导致 O(n^2))
                    let start = self.pos - 1;
                    let b0 = self.bytes[start];
                    let clen = if b0 < 0x80 {
                        1
                    } else if b0 < 0xE0 {
                        2
                    } else if b0 < 0xF0 {
                        3
                    } else {
                        4
                    };
                    if start + clen > self.bytes.len() {
                        return Err("JSON 字符串截断的 UTF-8".into());
                    }
                    let s = std::str::from_utf8(&self.bytes[start..start + clen])
                        .map_err(|_| "JSON 字符串非法 UTF-8".to_string())?;
                    out.push_str(s);
                    self.pos = start + clen;
                }
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos] as char;
            if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "JSON 数字非法".to_string())?;
        if text.is_empty() {
            return Err("JSON 非法数字".into());
        }
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("JSON 非法数字: {}", text))
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.pos += 1; // {
        let mut m = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("JSON 对象键必须是字符串".into());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("JSON 对象缺冒号".into());
            }
            self.pos += 1;
            let val = self.parse_value()?;
            m.insert(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(m));
                }
                _ => return Err("JSON 对象缺逗号或右括号".into()),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.pos += 1; // [
        let mut a = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(a));
        }
        loop {
            let val = self.parse_value()?;
            a.push(val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(a));
                }
                _ => return Err("JSON 数组缺逗号或右括号".into()),
            }
        }
    }
}

/// 解析 JSON 字符串
pub fn parse(s: &str) -> Result<Json, String> {
    let mut p = Parser::new(s);
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err("JSON 尾部有多余内容".into());
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let j = Json::build(vec![
            ("name", Json::str("本地搜索")),
            ("count", Json::num(42.0)),
            ("ok", Json::Bool(true)),
            ("tags", Json::arr(vec![Json::str("a"), Json::str("b")])),
            ("nested", Json::build(vec![("x", Json::Null)])),
        ]);
        let s = j.to_string();
        let back = parse(&s).unwrap();
        assert_eq!(back, j);
    }

    #[test]
    fn parse_escapes() {
        let j = parse(r#"{"a":"he\"llo\n\\","n":-3.5}"#).unwrap();
        assert_eq!(j.get("a").unwrap().as_str(), Some("he\"llo\n\\"));
        assert_eq!(j.get("n").unwrap().as_u64(), None);
        assert_eq!(j.get("n").unwrap(), &Json::Num(-3.5));
    }

    #[test]
    fn parse_unicode() {
        let j = parse(r#"{"k":"中\u6587"}"#).unwrap();
        assert_eq!(j.get("k").unwrap().as_str(), Some("中文"));
    }
}
