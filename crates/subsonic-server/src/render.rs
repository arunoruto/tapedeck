//! The response envelope in the three formats, and the JSON → Subsonic XML mapping.

use axum::{
    http::header,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

pub(crate) const API_VERSION: &str = "1.16.1";

/// The `f` parameter: XML unless the client asks for `json` or `jsonp` (with `callback`).
pub(crate) enum Format {
    Xml,
    Json,
    Jsonp(String),
}

impl Format {
    pub(crate) fn from_params(f: Option<&str>, callback: Option<&str>) -> Format {
        match f.unwrap_or("xml") {
            "json" => Format::Json,
            "jsonp" => Format::Jsonp(callback.unwrap_or("callback").to_string()),
            _ => Format::Xml,
        }
    }
}

pub(crate) struct Identity<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

/// `status="ok"` with `data` merged in, or `status="failed"` with the error.
pub(crate) fn envelope(fmt: &Format, who: &Identity<'_>, result: Result<Value, crate::Error>) -> Response {
    let mut base = json!({
        "version": API_VERSION,
        "type": who.name,
        "serverVersion": who.version,
        "openSubsonic": true,
    });
    let obj = base.as_object_mut().expect("object");
    match result {
        Ok(Value::Object(data)) => {
            obj.insert("status".into(), json!("ok"));
            obj.extend(data);
        }
        Ok(_) => {
            obj.insert("status".into(), json!("ok"));
        }
        Err(e) => {
            obj.insert("status".into(), json!("failed"));
            obj.insert("error".into(), json!({ "code": e.code, "message": e.message }));
        }
    }
    strip_nulls(&mut base);
    match fmt {
        Format::Json => {
            ([(header::CONTENT_TYPE, "application/json")], json!({ "subsonic-response": base }).to_string())
                .into_response()
        }
        Format::Jsonp(cb) => {
            let body = format!("{cb}({});", json!({ "subsonic-response": base }));
            ([(header::CONTENT_TYPE, "application/javascript")], body).into_response()
        }
        Format::Xml => {
            base["xmlns"] = json!("http://subsonic.org/restapi");
            let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            xml("subsonic-response", &base, &mut out);
            ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], out).into_response()
        }
    }
}

/// Some Android clients crash on a key that is present but null; leave optional fields out.
pub(crate) fn strip_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            m.values_mut().for_each(strip_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(strip_nulls),
        _ => {}
    }
}

/// The Subsonic XML shape from the JSON one: scalars become attributes, objects child elements,
/// arrays repeated child elements named after the key, arrays of scalars text elements.
pub(crate) fn xml(name: &str, v: &Value, out: &mut String) {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match v {
        Value::Object(m) => {
            out.push('<');
            out.push_str(name);
            let mut children = vec![];
            for (k, v) in m {
                match v {
                    Value::Null => {}
                    Value::Object(_) | Value::Array(_) => children.push((k, v)),
                    scalar => out.push_str(&format!(" {k}=\"{}\"", esc(&text(scalar)))),
                }
            }
            if children.is_empty() {
                out.push_str("/>");
                return;
            }
            out.push('>');
            for (k, v) in children {
                xml(k, v, out);
            }
            out.push_str(&format!("</{name}>"));
        }
        Value::Array(items) => {
            for item in items {
                xml(name, item, out);
            }
        }
        Value::Null => {}
        scalar => out.push_str(&format!("<{name}>{}</{name}>", esc(&text(scalar)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_follows_the_subsonic_shape() {
        let v = json!({ "status": "ok", "artists": { "ignoredArticles": "", "index": [
            { "name": "Z", "artist": [{ "id": "a1", "name": "Zed & Co", "albumCount": 2 }] } ] },
            "user": { "username": "x", "folder": [1, 2] }, "license": { "valid": true }, "nothing": null });
        let mut out = String::new();
        xml("subsonic-response", &v, &mut out);
        assert_eq!(
            out,
            "<subsonic-response status=\"ok\"><artists ignoredArticles=\"\"><index name=\"Z\">\
             <artist albumCount=\"2\" id=\"a1\" name=\"Zed &amp; Co\"/></index></artists>\
             <license valid=\"true\"/><user username=\"x\"><folder>1</folder><folder>2</folder></user></subsonic-response>"
        );
        assert!(matches!(Format::from_params(None, None), Format::Xml));
        assert!(matches!(Format::from_params(Some("json"), None), Format::Json));
    }
}
