//! Reading config files in scripts: `ferry.json`, `ferry.toml`, `ferry.yaml` and `ferry.xml`
//! turn text into Lua tables. They only read; writing a config back is the script's own text.

use super::*;
use crate::books::xml::{self, Kind};

mod yaml;

/// What a config file holds, before it becomes Lua values.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Data {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Data>),
    /// Keys in the file's order (Lua tables don't keep it, but duplicates are caught by it).
    Map(Vec<(String, Data)>),
}

impl Data {
    /// A scalar as text, for a key.
    fn text(&self) -> String {
        match self {
            Data::Null => String::new(),
            Data::Bool(b) => b.to_string(),
            Data::Int(n) => n.to_string(),
            Data::Float(f) => f.to_string(),
            Data::Str(s) => s.clone(),
            Data::List(_) | Data::Map(_) => String::new(),
        }
    }
}

/// `ferry.null`: what a null inside a list or map becomes, so the entry isn't lost (a nil there
/// would leave a hole in a list, and drop the key from a map).
pub(super) fn null() -> Value {
    Value::LightUserData(mlua::LightUserData(std::ptr::null_mut()))
}

/// `d` as a Lua value: a null on its own is nil, a null inside is `ferry.null`.
pub(super) fn to_lua(lua: &Lua, d: Data) -> mlua::Result<Value> {
    Ok(match d {
        Data::Null => Value::Nil,
        d => inner(lua, d)?,
    })
}

fn inner(lua: &Lua, d: Data) -> mlua::Result<Value> {
    Ok(match d {
        Data::Null => null(),
        Data::Bool(b) => Value::Boolean(b),
        Data::Int(n) => Value::Integer(n),
        Data::Float(f) => Value::Number(f),
        Data::Str(s) => Value::String(lua.create_string(&s)?),
        Data::List(xs) => {
            let t = lua.create_table_with_capacity(xs.len(), 0)?;
            for (i, x) in xs.into_iter().enumerate() {
                t.raw_set(i + 1, inner(lua, x)?)?;
            }
            Value::Table(t)
        }
        Data::Map(kv) => {
            let t = lua.create_table_with_capacity(0, kv.len())?;
            for (k, v) in kv {
                t.raw_set(k, inner(lua, v)?)?;
            }
            Value::Table(t)
        }
    })
}

/// The line `pos` is on in `text`, counted from 1.
fn line(text: &str, pos: usize) -> usize {
    xml::line_of(text, pos)
}

pub(super) fn json(text: &str) -> Result<Data, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| {
        let msg = e.to_string();
        let msg = msg.split(" at line ").next().unwrap_or(&msg).to_string();
        format!("line {}: {msg}", e.line())
    })?;
    Ok(from_json(v))
}

fn from_json(v: serde_json::Value) -> Data {
    use serde_json::Value as J;
    match v {
        J::Null => Data::Null,
        J::Bool(b) => Data::Bool(b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Data::Int(i),
            None => Data::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        J::String(s) => Data::Str(s),
        J::Array(xs) => Data::List(xs.into_iter().map(from_json).collect()),
        J::Object(kv) => Data::Map(kv.into_iter().map(|(k, v)| (k, from_json(v))).collect()),
    }
}

pub(super) fn toml(text: &str) -> Result<Data, String> {
    let doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
        let at = e.span().map_or(1, |s| line(text, s.start));
        format!("line {at}: {}", e.message().trim_end())
    })?;
    Ok(toml_table(doc.as_table()))
}

fn toml_table(t: &toml_edit::Table) -> Data {
    Data::Map(
        t.iter()
            .filter_map(|(k, v)| toml_item(v).map(|v| (k.to_string(), v)))
            .collect(),
    )
}

fn toml_item(i: &toml_edit::Item) -> Option<Data> {
    use toml_edit::Item;
    Some(match i {
        Item::None => return None,
        Item::Value(v) => toml_value(v),
        Item::Table(t) => toml_table(t),
        Item::ArrayOfTables(a) => Data::List(a.iter().map(toml_table).collect()),
    })
}

fn toml_value(v: &toml_edit::Value) -> Data {
    use toml_edit::Value as T;
    match v {
        T::String(s) => Data::Str(s.value().clone()),
        T::Integer(n) => Data::Int(*n.value()),
        T::Float(f) => Data::Float(*f.value()),
        T::Boolean(b) => Data::Bool(*b.value()),
        // Dates and times stay as written: "1979-05-27T07:32:00Z".
        T::Datetime(d) => Data::Str(d.value().to_string()),
        T::Array(a) => Data::List(a.iter().map(toml_value).collect()),
        T::InlineTable(t) => Data::Map(
            t.iter()
                .map(|(k, v)| (k.to_string(), toml_value(v)))
                .collect(),
        ),
    }
}

/// Every document in `text`.
pub(super) fn yaml(text: &str) -> Result<Vec<Data>, String> {
    yaml::parse(text)
}

/// The root element as `{tag = "name", attr = {...}, child, "text", ...}`: the children in
/// order after it, elements as tables of their own and text as strings. Text that's only spaces
/// between tags is left out; comments, `<?…?>` and the doctype are too.
pub(super) fn xml(text: &str) -> Result<Data, String> {
    let plain = |e: String| match e.strip_prefix("Line ") {
        Some(r) => format!("line {r}"),
        None => e,
    };
    xml::check(text).map_err(plain)?;
    let toks = xml::tokenize(text).map_err(plain)?;
    // Open elements: (tag, attrs, children).
    let mut stack: Vec<(String, Data, Vec<Data>)> = vec![];
    let mut root = None;
    let element = |tag: String, attr: Data, kids: Vec<Data>| {
        let mut kv = vec![
            ("tag".to_string(), Data::Str(tag)),
            ("attr".to_string(), attr),
        ];
        // Children go at 1, 2, …: their keys as text here, made numbers in `xml_lua`.
        kv.extend(
            kids.into_iter()
                .enumerate()
                .map(|(i, k)| (format!("#{}", i + 1), k)),
        );
        Data::Map(kv)
    };
    let mut close = |stack: &mut Vec<(String, Data, Vec<Data>)>, done: Data| match stack.last_mut()
    {
        Some(p) => p.2.push(done),
        None => root = Some(done),
    };
    for t in &toks {
        let attrs = || {
            Data::Map(
                t.attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), Data::Str(v.clone())))
                    .collect(),
            )
        };
        match t.kind {
            Kind::Open => stack.push((t.name.clone(), attrs(), vec![])),
            Kind::Empty => close(&mut stack, element(t.name.clone(), attrs(), vec![])),
            Kind::Close => {
                if let Some((tag, attr, kids)) = stack.pop() {
                    close(&mut stack, element(tag, attr, kids));
                }
            }
            Kind::Text | Kind::CData => {
                let raw = &text[t.start..t.end];
                let s = if t.kind == Kind::CData {
                    raw["<![CDATA[".len()..raw.len() - "]]>".len()].to_string()
                } else if raw.trim().is_empty() {
                    continue;
                } else {
                    xml::unescape(raw)
                };
                if let Some(p) = stack.last_mut() {
                    // Text next to text (as around a CDATA) reads as one.
                    if let Some(Data::Str(prev)) = p.2.last_mut() {
                        prev.push_str(&s);
                    } else {
                        p.2.push(Data::Str(s));
                    }
                }
            }
            Kind::Comment | Kind::Pi | Kind::Doctype => {}
        }
    }
    root.ok_or_else(|| "line 1: there's no element in it".to_string())
}

/// An element from `xml` as a Lua table, with its children at 1, 2, ….
pub(super) fn xml_lua(lua: &Lua, d: Data) -> mlua::Result<Value> {
    let Data::Map(kv) = d else {
        return to_lua(lua, d);
    };
    let t = lua.create_table()?;
    for (k, v) in kv {
        let v = match v {
            v @ Data::Map(_) if k.starts_with('#') => xml_lua(lua, v)?,
            v => inner(lua, v)?,
        };
        match k.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
            Some(n) => t.raw_set(n, v)?,
            None => t.raw_set(k, v)?,
        }
    }
    Ok(Value::Table(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Data::*;

    fn s(t: &str) -> Data {
        Str(t.to_string())
    }

    #[test]
    fn json_and_its_errors() {
        assert_eq!(
            json("{\"a\": [1, 2.5, null, \"x\"], \"b\": {\"c\": true}}").unwrap(),
            Map(vec![
                ("a".into(), List(vec![Int(1), Float(2.5), Null, s("x")])),
                ("b".into(), Map(vec![("c".into(), Bool(true))])),
            ])
        );
        let e = json("{\n  \"a\": 1,\n  \"b\" 2\n}").unwrap_err();
        assert!(e.starts_with("line 3: "), "{e}");
        assert!(!e.contains(" at line "), "{e}");
    }

    #[test]
    fn toml_and_its_errors() {
        let d = toml(
            "title = \"x\"\n[owner]\nborn = 1979-05-27T07:32:00Z\n[[fruit]]\nname = \"apple\"\n\
             [[fruit]]\nname = \"pear\"\nsizes = [1, 2]\npt = { x = 1.5 }\n",
        )
        .unwrap();
        assert_eq!(
            d,
            Map(vec![
                ("title".into(), s("x")),
                (
                    "owner".into(),
                    Map(vec![("born".into(), s("1979-05-27T07:32:00Z"))])
                ),
                (
                    "fruit".into(),
                    List(vec![
                        Map(vec![("name".into(), s("apple"))]),
                        Map(vec![
                            ("name".into(), s("pear")),
                            ("sizes".into(), List(vec![Int(1), Int(2)])),
                            ("pt".into(), Map(vec![("x".into(), Float(1.5))])),
                        ]),
                    ])
                ),
            ])
        );
        let e = toml("a = 1\nb = \n").unwrap_err();
        assert!(e.starts_with("line 2: "), "{e}");
    }

    #[test]
    fn xml_as_tables() {
        let d = xml(
            "<?xml version=\"1.0\"?>\n<!-- c -->\n<conf v=\"2\">\n  <name>A &amp; B</name>\n  \
             <empty/>\n  <code><![CDATA[x < y]]></code>\n</conf>\n",
        )
        .unwrap();
        let el = |tag: &str, attr: Vec<(&str, &str)>, kids: Vec<Data>| {
            let mut kv = vec![
                ("tag".to_string(), s(tag)),
                (
                    "attr".to_string(),
                    Map(attr.into_iter().map(|(k, v)| (k.into(), s(v))).collect()),
                ),
            ];
            kv.extend(
                kids.into_iter()
                    .enumerate()
                    .map(|(i, k)| (format!("#{}", i + 1), k)),
            );
            Map(kv)
        };
        assert_eq!(
            d,
            el(
                "conf",
                vec![("v", "2")],
                vec![
                    el("name", vec![], vec![s("A & B")]),
                    el("empty", vec![], vec![]),
                    el("code", vec![], vec![s("x < y")]),
                ]
            )
        );
        let e = xml("<a>\n<b>\n</a>").unwrap_err();
        assert!(e.starts_with("line "), "{e}");
    }
}
