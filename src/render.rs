//! Human output for `data` events. `--json` bypasses all of this.

use serde_json::Value;

/// Columns worth a terminal's width, in display order. Anything else (MBIDs, URLs, errors) is
/// still in `--json`.
const COLUMNS: &[&str] = &[
    "id",
    "kind",
    "date",
    "status",
    "artist",
    "title",
    "album",
    "year",
    "track_no",
    "name",
    "track_count",
    "tracks",
    "smart",
    "synced",
    "fetched_at",
    "path",
];

pub fn render(v: &Value) -> String {
    match v {
        Value::Array(rows) if rows.iter().all(Value::is_object) && !rows.is_empty() => table(rows),
        Value::Array(rows) if rows.is_empty() => "(nothing)".into(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                Value::Array(rows) if rows.iter().all(Value::is_object) && !rows.is_empty() => {
                    format!("{k}:\n{}", table(rows))
                }
                Value::Object(inner) => {
                    format!(
                        "{k}: {}",
                        inner.iter().map(|(k, v)| format!("{k} {}", cell(v))).collect::<Vec<_>>().join(", ")
                    )
                }
                other => format!("{k}: {}", cell(other)),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => cell(other),
    }
}

fn table(rows: &[Value]) -> String {
    let cols: Vec<&str> =
        COLUMNS.iter().copied().filter(|c| rows.iter().any(|r| r.get(c).is_some_and(|v| !v.is_null()))).collect();
    let cells: Vec<Vec<String>> =
        rows.iter().map(|r| cols.iter().map(|c| cell(r.get(c).unwrap_or(&Value::Null))).collect()).collect();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| cells.iter().map(|r| r[i].chars().count()).chain([c.len()]).max().unwrap_or(0))
        .collect();
    let line = |items: Vec<String>| {
        items
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{s:<w$}", w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = vec![line(cols.iter().map(|c| c.to_uppercase()).collect())];
    out.extend(cells.into_iter().map(line));
    out.join("\n")
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(true) => "yes".into(),
        Value::Bool(false) => String::new(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                // playlist_syncs rows: show where it went
                Value::Object(o) => o.get("target").map(cell).unwrap_or_else(|| i.to_string()),
                other => cell(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tables_and_objects() {
        let t = render(&json!([
            {"id": 1, "artist": "Bicep", "title": "Apricots", "status": "ready", "recording_mbid": "x", "year": 2021},
            {"id": 12, "artist": "Bonobo", "title": "Polyghost", "status": "wanted", "recording_mbid": "y", "year": null}
        ]));
        assert_eq!(t.lines().next().unwrap(), "ID  STATUS  ARTIST  TITLE      YEAR");
        assert!(t.contains("12  wanted  Bonobo  Polyghost"));
        assert!(!t.contains("recording_mbid"));
        assert_eq!(
            render(&json!({"playlists": 1, "tracks": {"failed": 4, "ready": 46}})),
            "playlists: 1\ntracks: failed 4, ready 46"
        );
        assert_eq!(render(&json!([])), "(nothing)");
        let r = render(
            &json!({"target": "plex", "playlists": [{"id": "7", "title": "Weekly Jams", "tracks": 50, "smart": false}]}),
        );
        assert!(r.starts_with("playlists:\nID  TITLE        TRACKS") && r.ends_with("target: plex"), "{r}");
    }
}
