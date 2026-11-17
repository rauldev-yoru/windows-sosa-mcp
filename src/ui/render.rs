//! Snapshot node classification and text rendering.
//!
//! The rendered tree is the contract the model reads, so its format is
//! stable: `  `-indented `[Role] "name" id=… {patterns}
//! [flags] [ref=eN]`, names JSON-quoted (`json.dumps(ensure_ascii=False)`),
//! numbers with printf `%g`.

use serde_json::{Map, Value, json};

/// Pixel rectangle (virtual-desktop coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl Bounds {
    pub fn to_json(self) -> Value {
        json!({"x": self.x, "y": self.y, "width": self.width, "height": self.height})
    }
}

/// Properties of one UIA element as read from the cache.
#[derive(Debug, Clone, Default)]
pub struct RawNode {
    pub role: String,
    pub name: String,
    pub automation_id: String,
    pub class: String,
    pub enabled: bool,
    pub offscreen: bool,
    pub focusable: bool,
    pub focused: bool,
    pub password: bool,
    pub patterns: Vec<&'static str>,
    pub bounds: Option<Bounds>,
    pub runtime_id: Vec<i32>,
    pub state: Map<String, Value>,
}

/// A node of the snapshot after classification.
#[derive(Debug, Clone, Default)]
pub struct Node {
    pub depth: usize,
    pub role: String,
    pub name: String,
    pub automation_id: String,
    pub class: String,
    pub enabled: bool,
    pub offscreen: bool,
    pub focusable: bool,
    pub focused: bool,
    pub password: bool,
    pub patterns: Vec<&'static str>,
    pub bounds: Option<Bounds>,
    pub ref_: Option<String>,
    pub runtime_id: Vec<i32>,
    pub state: Option<Map<String, Value>>,
    pub hidden: bool,
}

/// Roles that get a ref when named (useful as `scope=ref` anchors).
const CONTAINER_ROLES: [&str; 13] = [
    "Group", "Pane", "List", "Tree", "Table", "DataGrid", "Menu", "MenuBar", "ToolBar", "Tab",
    "Document", "Custom", "Window",
];

/// PowerShell `$name.Length -gt 200` / `Substring(0,200)+'…'` (UTF-16 units).
pub fn truncate_name(name: &str) -> String {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() <= 200 {
        return name.to_string();
    }
    let mut s = String::from_utf16_lossy(&units[..200]);
    s.push('…');
    s
}

/// `IndexOf(needle, OrdinalIgnoreCase) >= 0`.
pub fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    hay.to_uppercase().contains(&needle.to_uppercase())
}

/// Ref numbering and `name_contains` filtering across one snapshot.
#[derive(Debug, Default)]
pub struct Classifier {
    refs: usize,
    needle: Option<String>,
}

impl Classifier {
    pub fn new(needle: Option<String>) -> Self {
        Self {
            refs: 0,
            needle: needle.filter(|n| !n.is_empty()),
        }
    }

    /// The worker's per-node rules: container/actionable, ref + runtime id +
    /// state for actionable nodes (and the root), password redaction and
    /// `name_contains` hiding.
    pub fn node(&mut self, raw: RawNode, depth: usize) -> Node {
        let name = truncate_name(&raw.name);
        let role = raw.role;
        let container = CONTAINER_ROLES.contains(&role.as_str())
            && (!name.is_empty() || !raw.automation_id.is_empty());
        let pats = raw.patterns;
        let actionable = (!pats.is_empty() && (pats.iter().any(|p| *p != "window") || depth == 0))
            || raw.focusable
            || matches!(role.as_str(), "Edit" | "Document" | "ComboBox")
            || container;
        let mut node = Node {
            depth,
            role,
            name,
            automation_id: raw.automation_id,
            class: raw.class,
            enabled: raw.enabled,
            offscreen: raw.offscreen,
            focusable: raw.focusable,
            focused: raw.focused,
            password: raw.password,
            patterns: pats,
            bounds: raw.bounds,
            ..Node::default()
        };
        if actionable || depth == 0 {
            self.refs += 1;
            node.ref_ = Some(format!("e{}", self.refs));
            node.runtime_id = raw.runtime_id;
            let mut state = raw.state;
            if node.password && state.contains_key("value") {
                state.insert("value".into(), json!("<redacted:password>"));
            }
            node.state = Some(state);
        }
        if let Some(n) = &self.needle
            && !contains_ignore_case(&node.name, n)
            && depth > 0
            && node.ref_.is_none()
        {
            node.hidden = true;
        }
        node
    }
}

/// printf-style `%g` of `v`.
pub fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf" } else { "-inf" }.into();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    let sci = format!("{v:.5e}");
    let (mant, exp) = sci.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent digits");
    let strip = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mant), exp.abs())
    } else {
        let decimals = (5 - exp) as usize;
        strip(&format!("{v:.decimals$}"))
    }
}

/// A JSON number as PowerShell `ConvertTo-Json` writes a double: integral
/// values without a fraction (`1`, not `1.0`).
pub fn num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

/// .NET `Math.Round(v, digits)` (MidpointRounding.ToEven).
pub fn round_dp(v: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    (v * p).round_ties_even() / p
}

/// PowerShell `[int]$double` (banker's rounding).
pub fn ps_int(v: f64) -> i64 {
    v.round_ties_even() as i64
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes")
}

fn as_f64(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).unwrap_or(0.0)
}

/// Render the compact or full text tree of a snapshot.
pub fn render_tree(nodes: &[Node], detail: &str) -> String {
    let full = detail == "full";
    let empty = Map::new();
    let mut lines = Vec::new();
    for n in nodes {
        if n.hidden {
            continue;
        }
        let mut parts = vec![format!("{}[{}]", "  ".repeat(n.depth), n.role)];
        if !n.name.is_empty() {
            parts.push(json_str(&n.name));
        }
        if !n.automation_id.is_empty() && (full || n.ref_.is_some()) {
            parts.push(format!("id={}", n.automation_id));
        }
        let st = n.state.as_ref().unwrap_or(&empty);
        let mut bits = Vec::new();
        for p in n
            .patterns
            .iter()
            .filter(|p| **p != "window" || n.depth == 0)
        {
            match *p {
                "value" if st.contains_key("value") => {
                    let v = st["value"].as_str().unwrap_or_default();
                    let mut cut: String = v.chars().take(120).collect();
                    if v.chars().count() > 120 {
                        cut.push('…');
                    }
                    bits.push(format!("value={}", json_str(&cut)));
                }
                "toggle" if st.contains_key("toggle") => {
                    bits.push(format!(
                        "toggle={}",
                        st["toggle"].as_str().unwrap_or_default()
                    ));
                }
                "expand" if st.contains_key("expand") => {
                    bits.push(format!(
                        "expand={}",
                        st["expand"].as_str().unwrap_or_default()
                    ));
                }
                "select" if st.contains_key("selected") => bits.push(
                    if st["selected"].as_bool().unwrap_or(false) {
                        "selected"
                    } else {
                        "select"
                    }
                    .to_string(),
                ),
                "range" if st.contains_key("range_value") => bits.push(format!(
                    "range={}/{}..{}",
                    fmt_g(as_f64(st.get("range_value"))),
                    fmt_g(as_f64(st.get("range_min"))),
                    fmt_g(as_f64(st.get("range_max")))
                )),
                "window" if st.contains_key("window_state") => bits.push(format!(
                    "window={}",
                    st["window_state"].as_str().unwrap_or_default()
                )),
                "scroll"
                    if st
                        .get("scroll_vertical_percent")
                        .is_some_and(|v| !v.is_null()) =>
                {
                    bits.push(format!(
                        "scroll_v={}%",
                        fmt_g(as_f64(st.get("scroll_vertical_percent")))
                    ));
                }
                other => bits.push(other.to_string()),
            }
        }
        if !bits.is_empty() {
            parts.push(format!("{{{}}}", bits.join(" ")));
        }
        let mut flags = Vec::new();
        if n.focused {
            flags.push("focused");
        }
        if !n.enabled {
            flags.push("disabled");
        }
        if n.offscreen {
            flags.push("offscreen");
        }
        if n.password {
            flags.push("password");
        }
        if st
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            flags.push("readonly");
        }
        parts.extend(flags.iter().map(|f| format!("[{f}]")));
        if full && let Some(b) = n.bounds {
            parts.push(format!("[box={},{},{},{}]", b.x, b.y, b.width, b.height));
        }
        if let Some(r) = &n.ref_ {
            parts.push(format!("[ref={r}]"));
        }
        lines.push(
            parts
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    lines.join("\n")
}

/// The `elements` entry of a ref node.
pub fn element(n: &Node) -> Value {
    json!({
        "ref": n.ref_,
        "role": n.role,
        "name": n.name,
        "automation_id": if n.automation_id.is_empty() { Value::Null } else { json!(n.automation_id) },
        "patterns": n.patterns,
        "focusable": n.focusable,
        "focused": n.focused,
        "enabled": n.enabled,
        "password": n.password,
        "state": n.state.clone().unwrap_or_default(),
        "bounds": n.bounds.map(Bounds::to_json),
        "depth": n.depth,
    })
}

/// UIA control type ids (`UIA_*ControlTypeId`) by role (unknown roles map
/// to Custom = 50025).
pub const ROLES: [(&str, i32); 41] = [
    ("Button", 50000),
    ("Calendar", 50001),
    ("CheckBox", 50002),
    ("ComboBox", 50003),
    ("Edit", 50004),
    ("Hyperlink", 50005),
    ("Image", 50006),
    ("ListItem", 50007),
    ("List", 50008),
    ("Menu", 50009),
    ("MenuBar", 50010),
    ("MenuItem", 50011),
    ("ProgressBar", 50012),
    ("RadioButton", 50013),
    ("ScrollBar", 50014),
    ("Slider", 50015),
    ("Spinner", 50016),
    ("StatusBar", 50017),
    ("Tab", 50018),
    ("TabItem", 50019),
    ("Text", 50020),
    ("ToolBar", 50021),
    ("ToolTip", 50022),
    ("Tree", 50023),
    ("TreeItem", 50024),
    ("Custom", 50025),
    ("Group", 50026),
    ("Thumb", 50027),
    ("DataGrid", 50028),
    ("DataItem", 50029),
    ("Document", 50030),
    ("SplitButton", 50031),
    ("Window", 50032),
    ("Pane", 50033),
    ("Header", 50034),
    ("HeaderItem", 50035),
    ("Table", 50036),
    ("TitleBar", 50037),
    ("Separator", 50038),
    ("SemanticZoom", 50039),
    ("AppBar", 50040),
];

/// Role name of a control type id (`ControlType.ProgrammaticName` minus
/// the `ControlType.` prefix).
pub fn role_name(id: i32) -> String {
    ROLES
        .iter()
        .find(|(_, i)| *i == id)
        .map_or_else(|| "Custom".to_string(), |(n, _)| (*n).to_string())
}

/// Control type id of a role, Custom (50025) when unknown (SemanticZoom and
/// AppBar anchor as Custom).
pub fn control_type_id(role: &str) -> i32 {
    ROLES
        .iter()
        .take(39)
        .find(|(n, _)| *n == role)
        .map_or(50025, |(_, i)| *i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(role: &str, name: &str, pats: &[&'static str]) -> RawNode {
        RawNode {
            role: role.into(),
            name: name.into(),
            enabled: true,
            patterns: pats.to_vec(),
            runtime_id: vec![42, 1],
            ..RawNode::default()
        }
    }

    #[test]
    fn g_format() {
        for (v, s) in [
            (0.0, "0"),
            (1.0, "1"),
            (100.0, "100"),
            (33.3, "33.3"),
            (12.5, "12.5"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123456.0, "123456"),
            (1234567.0, "1.23457e+06"),
            (-2.5, "-2.5"),
            (1e20, "1e+20"),
            (0.1 + 0.2, "0.3"),
            (99.99999, "100"),
        ] {
            assert_eq!(fmt_g(v), s, "{v}");
        }
    }

    #[test]
    fn rounding_is_bankers() {
        assert_eq!(ps_int(2.5), 2);
        assert_eq!(ps_int(3.5), 4);
        assert_eq!(ps_int(-0.5), 0);
        assert_eq!(round_dp(1280.0 / 2560.0, 4), 0.5);
        assert_eq!(round_dp(1280.0 / 1921.0, 4), 0.6663);
        assert_eq!(num(1.0), json!(1));
        assert_eq!(num(0.5), json!(0.5));
    }

    #[test]
    fn classification_and_refs() {
        let mut c = Classifier::new(None);
        let root = c.node(raw("Window", "App", &["window"]), 0);
        assert_eq!(root.ref_.as_deref(), Some("e1"));
        let pane = c.node(raw("Pane", "", &[]), 1);
        assert!(pane.ref_.is_none());
        let named_pane = c.node(raw("Pane", "Toolbar", &[]), 1);
        assert_eq!(named_pane.ref_.as_deref(), Some("e2"));
        let win_child = c.node(raw("Window", "", &["window"]), 1);
        assert!(
            win_child.ref_.is_none(),
            "window pattern alone is not actionable below root"
        );
        let edit = c.node(raw("Edit", "", &[]), 2);
        assert_eq!(edit.ref_.as_deref(), Some("e3"));
        let mut pw = raw("Edit", "Clave", &["value"]);
        pw.password = true;
        pw.state.insert("value".into(), json!("hunter2"));
        let pw = c.node(pw, 2);
        assert_eq!(pw.state.as_ref().unwrap()["value"], "<redacted:password>");
        let long = c.node(raw("Text", &"x".repeat(250), &[]), 1);
        assert_eq!(long.name.chars().count(), 201);
        assert!(long.name.ends_with('…'));
    }

    #[test]
    fn name_contains_hides_only_unreffed_nodes() {
        let mut c = Classifier::new(Some("guardar".into()));
        let root = c.node(raw("Window", "Editor", &["window"]), 0);
        let text = c.node(raw("Text", "Etiqueta", &[]), 1);
        let text2 = c.node(raw("Text", "GUARDAR cambios", &[]), 1);
        let button = c.node(raw("Button", "Cancelar", &["invoke"]), 1);
        assert!(!root.hidden && text.hidden && !text2.hidden && !button.hidden);
        let tree = render_tree(&[root, text, text2, button], "compact");
        assert_eq!(
            tree,
            "[Window] \"Editor\" {window} [ref=e1]\n  [Text] \"GUARDAR cambios\"\n  [Button] \"Cancelar\" {invoke} [ref=e2]"
        );
    }

    #[test]
    fn render_format() {
        let mut c = Classifier::new(None);
        let mut w = raw("Window", "Bloc \"de\" notas", &["window"]);
        w.state.insert("window_state".into(), json!("normal"));
        w.automation_id = "Main".into();
        w.bounds = Some(Bounds {
            x: -2560,
            y: -1440,
            width: 800,
            height: 600,
        });
        let mut e = raw("Edit", "Texto", &["value", "scroll"]);
        e.automation_id = "txt".into();
        e.focused = true;
        e.state
            .insert("value".into(), json!(format!("{}\n", "a".repeat(130))));
        e.state.insert("read_only".into(), json!(false));
        e.state
            .insert("scroll_vertical_percent".into(), json!(12.5));
        let mut cb = raw("CheckBox", "Ajuste", &["toggle"]);
        cb.state.insert("toggle".into(), json!("on"));
        cb.enabled = false;
        let mut sl = raw("Slider", "Vol", &["range", "value"]);
        sl.state.insert("range_value".into(), num(5.0));
        sl.state.insert("range_min".into(), num(0.0));
        sl.state.insert("range_max".into(), num(10.0));
        let mut li = raw("ListItem", "Uno", &["select", "scroll"]);
        li.state.insert("selected".into(), json!(true));
        li.state
            .insert("scroll_vertical_percent".into(), Value::Null);
        let mut t = raw("Text", "plain", &[]);
        t.automation_id = "lbl".into();
        t.class = "Static".into();
        t.bounds = Some(Bounds {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        let nodes = vec![
            c.node(w, 0),
            c.node(e, 1),
            c.node(cb, 1),
            c.node(sl, 1),
            c.node(li, 2),
            c.node(t, 1),
        ];
        let compact = render_tree(&nodes, "compact");
        let want = format!(
            "[Window] \"Bloc \\\"de\\\" notas\" id=Main {{window=normal}} [ref=e1]\n  \
             [Edit] \"Texto\" id=txt {{value=\"{}…\" scroll_v=12.5%}} [focused] [ref=e2]\n  \
             [CheckBox] \"Ajuste\" {{toggle=on}} [disabled] [ref=e3]\n  \
             [Slider] \"Vol\" {{range=5/0..10 value}} [ref=e4]\n    \
             [ListItem] \"Uno\" {{selected scroll}} [ref=e5]\n  \
             [Text] \"plain\"",
            "a".repeat(120)
        );
        assert_eq!(compact, want);
        let full = render_tree(&nodes, "full");
        assert!(full.starts_with(
            "[Window] \"Bloc \\\"de\\\" notas\" id=Main {window=normal} [box=-2560,-1440,800,600] [ref=e1]"
        ));
        assert!(full.ends_with("  [Text] \"plain\" id=lbl [box=1,2,3,4]"));
        let el = element(&nodes[1]);
        assert_eq!(el["ref"], "e2");
        assert_eq!(el["automation_id"], "txt");
        assert_eq!(el["bounds"], Value::Null);
        assert_eq!(element(&nodes[2])["automation_id"], Value::Null);
    }

    #[test]
    fn control_types() {
        assert_eq!(role_name(50033), "Pane");
        assert_eq!(role_name(12345), "Custom");
        assert_eq!(control_type_id("Separator"), 50038);
        assert_eq!(control_type_id("AppBar"), 50025);
        assert_eq!(control_type_id("Nope"), 50025);
    }
}
