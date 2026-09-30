//! The `.con` / `.tweak` / `.inc` / `.ai` script language.
//!
//! BF2 describes nearly everything in these line-oriented scripts:
//!
//! ```text
//! rem comment
//! ObjectTemplate.create SimpleObject gas_station
//! ObjectTemplate.geometry gas_station
//! include gas_station.tweak
//! if v_arg1 == host
//!   Object.create CPNAME_SK_64_hotel
//!   Object.absolutePosition -134.000/167.293/-277.000
//! endIf
//! ```
//!
//! We interpret the scripts once, at import time, into a [`World`]: a store of object
//! templates, geometry/collision references, placed instances and every other command in
//! order. Nothing here executes at game runtime.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use crate::vfs::{Vfs, normalize};

// ---------------------------------------------------------------------------------------
// Lexing and parsing
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub text: String,
    /// Written as `"..."`; quoted tokens are never variable-substituted.
    pub quoted: bool,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub line: u32,
    pub tokens: Vec<Token>,
    /// `command args -> v_result`
    pub assign: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Node {
    Stmt(Stmt),
    If {
        line: u32,
        branches: Vec<(Vec<Token>, Vec<Node>)>,
        else_body: Option<Vec<Node>>,
    },
    While {
        line: u32,
        cond: Vec<Token>,
        body: Vec<Node>,
    },
}

/// Splits a line into whitespace-separated tokens; `"..."` groups without escapes. Only ASCII
/// whitespace separates, as in BF2: AIX 2's Damocles names spawners `Pagoda<U+00A0>_1` (a
/// non-breaking space), which Unicode whitespace splitting turned into extra `Pagoda` flags.
pub fn tokenize_line(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' {
            let end = chars[i + 1..]
                .iter()
                .position(|&c| c == '"')
                .map_or(chars.len(), |p| i + 1 + p);
            tokens.push(Token {
                text: chars[i + 1..end].iter().collect(),
                quoted: true,
            });
            i = end + 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && !chars[j].is_ascii_whitespace() {
            if chars[j] == '"' {
                // Embedded quote (`a="b c"`): read through the closing quote.
                j = chars[j + 1..]
                    .iter()
                    .position(|&c| c == '"')
                    .map_or(chars.len(), |p| j + 1 + p + 1);
                continue;
            }
            j += 1;
        }
        tokens.push(Token {
            text: chars[i..j.min(chars.len())].iter().collect(),
            quoted: false,
        });
        i = j;
    }
    tokens
}

/// Text to statements, with `rem` lines and `beginRem`/`endRem` blocks removed.
pub fn lex(text: &str) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut rem_depth = 0u32;
    for (index, raw) in text.lines().enumerate() {
        let mut tokens = tokenize_line(raw);
        let Some(first) = tokens.first() else {
            continue;
        };
        let keyword = first.text.to_ascii_lowercase();
        match keyword.as_str() {
            "beginrem" => {
                rem_depth += 1;
                continue;
            }
            "endrem" => {
                rem_depth = rem_depth.saturating_sub(1);
                continue;
            }
            _ if rem_depth > 0 || keyword == "rem" => continue,
            _ => {}
        }
        let mut assign = None;
        if tokens.len() >= 3 && tokens[tokens.len() - 2].text == "->" {
            assign = tokens.pop().map(|t| t.text);
            tokens.pop();
        }
        out.push(Stmt {
            line: index as u32 + 1,
            tokens,
            assign,
        });
    }
    out
}

/// Builds the if/while block structure. Stray block keywords are reported and skipped.
pub fn parse(stmts: Vec<Stmt>, warnings: &mut Vec<String>) -> Vec<Node> {
    enum Frame {
        Root(Vec<Node>),
        If {
            line: u32,
            branches: Vec<(Vec<Token>, Vec<Node>)>,
            else_body: Option<Vec<Node>>,
        },
        While {
            line: u32,
            cond: Vec<Token>,
            body: Vec<Node>,
        },
    }
    fn body(frame: &mut Frame) -> &mut Vec<Node> {
        match frame {
            Frame::Root(body) => body,
            Frame::If {
                branches,
                else_body,
                ..
            } => match else_body {
                Some(body) => body,
                None => &mut branches.last_mut().expect("if has a branch").1,
            },
            Frame::While { body, .. } => body,
        }
    }
    fn close(frame: Frame) -> Node {
        match frame {
            Frame::If {
                line,
                branches,
                else_body,
            } => Node::If {
                line,
                branches,
                else_body,
            },
            Frame::While { line, cond, body } => Node::While { line, cond, body },
            Frame::Root(_) => unreachable!("root is never closed"),
        }
    }

    let mut stack = vec![Frame::Root(Vec::new())];
    for stmt in stmts {
        let keyword = stmt.tokens[0].text.to_ascii_lowercase();
        let rest = || stmt.tokens[1..].to_vec();
        match keyword.as_str() {
            "if" => stack.push(Frame::If {
                line: stmt.line,
                branches: vec![(rest(), Vec::new())],
                else_body: None,
            }),
            "elseif" => match stack.last_mut() {
                Some(Frame::If {
                    branches,
                    else_body: None,
                    ..
                }) => branches.push((rest(), Vec::new())),
                _ => warnings.push(format!("line {}: stray elseIf", stmt.line)),
            },
            "else" => match stack.last_mut() {
                Some(Frame::If { else_body, .. }) if else_body.is_none() => {
                    *else_body = Some(Vec::new())
                }
                _ => warnings.push(format!("line {}: stray else", stmt.line)),
            },
            "endif" => {
                if matches!(stack.last(), Some(Frame::If { .. })) {
                    let node = close(stack.pop().expect("checked"));
                    body(stack.last_mut().expect("root remains")).push(node);
                } else {
                    warnings.push(format!("line {}: stray endIf", stmt.line));
                }
            }
            "while" => stack.push(Frame::While {
                line: stmt.line,
                cond: rest(),
                body: Vec::new(),
            }),
            "endwhile" => {
                if matches!(stack.last(), Some(Frame::While { .. })) {
                    let node = close(stack.pop().expect("checked"));
                    body(stack.last_mut().expect("root remains")).push(node);
                } else {
                    warnings.push(format!("line {}: stray endWhile", stmt.line));
                }
            }
            _ => body(stack.last_mut().expect("root remains")).push(Node::Stmt(stmt)),
        }
    }
    // Close unterminated blocks rather than dropping their contents.
    while stack.len() > 1 {
        warnings.push("unterminated block".into());
        let node = close(stack.pop().expect("len > 1"));
        body(stack.last_mut().expect("root remains")).push(node);
    }
    match stack.pop() {
        Some(Frame::Root(body)) => body,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------------------

/// Parses `a/b/c` into floats.
pub fn parse_vec(s: &str) -> Option<Vec<f32>> {
    s.split('/').map(|p| p.trim().parse::<f32>().ok()).collect()
}

pub fn parse_vec3(s: &str) -> Option<[f32; 3]> {
    let v = parse_vec(s)?;
    (v.len() >= 3).then(|| [v[0], v[1], v[2]])
}

/// Parses `[a/b/c/d][e/f/g/h][..][..]` into rows.
pub fn parse_matrix(s: &str) -> Option<[[f32; 4]; 4]> {
    let rows: Vec<Vec<f32>> = s
        .split('[')
        .filter_map(|part| part.split(']').next())
        .filter(|part| !part.is_empty())
        .map(parse_vec)
        .collect::<Option<_>>()?;
    if rows.len() != 4 || rows.iter().any(|r| r.len() != 4) {
        return None;
    }
    let mut m = [[0.0; 4]; 4];
    for (i, row) in rows.iter().enumerate() {
        m[i].copy_from_slice(row);
    }
    Some(m)
}

// ---------------------------------------------------------------------------------------
// The world the scripts describe
// ---------------------------------------------------------------------------------------

/// Where a command came from, for diagnostics.
pub type Source = Arc<str>;

/// `ObjectTemplate.create <ty> <name>` plus everything set on it.
#[derive(Clone, Debug)]
pub struct Template {
    /// Type as written, e.g. `SimpleObject`, `PlayerControlObject`.
    pub ty: String,
    pub name: String,
    pub source: Source,
    /// Every property in order (method lowercased). Repeated properties are lists.
    pub props: Vec<(String, Vec<String>)>,
    pub children: Vec<ChildRef>,
    /// Name of the geometry template (`ObjectTemplate.geometry`).
    pub geometry: Option<String>,
    /// Name of the collision template (`ObjectTemplate.collisionMesh`).
    pub collision_mesh: Option<String>,
}

impl Template {
    /// The last value of a property.
    pub fn get(&self, method: &str) -> Option<&[String]> {
        let method = method.to_ascii_lowercase();
        self.props
            .iter()
            .rev()
            .find(|(m, _)| *m == method)
            .map(|(_, a)| a.as_slice())
    }

    /// All values of a repeated property, in order.
    pub fn get_all<'a>(&'a self, method: &str) -> impl Iterator<Item = &'a [String]> + 'a {
        let method = method.to_ascii_lowercase();
        self.props
            .iter()
            .filter(move |(m, _)| *m == method)
            .map(|(_, a)| a.as_slice())
    }

    pub fn get_str(&self, method: &str) -> Option<&str> {
        self.get(method)?.first().map(String::as_str)
    }

    pub fn get_f32(&self, method: &str) -> Option<f32> {
        self.get_str(method)?.parse().ok()
    }

    pub fn get_vec3(&self, method: &str) -> Option<[f32; 3]> {
        parse_vec3(self.get_str(method)?)
    }
}

/// A child slot added with `ObjectTemplate.addTemplate`.
#[derive(Clone, Debug)]
pub struct ChildRef {
    pub template: String,
    pub position: Option<[f32; 3]>,
    /// Yaw/pitch/roll in degrees.
    pub rotation: Option<[f32; 3]>,
}

/// `GeometryTemplate.create <ty> <name>`.
#[derive(Clone, Debug)]
pub struct Geometry {
    /// `StaticMesh`, `BundledMesh`, `SkinnedMesh`, `RoadCompiled`, ...
    pub ty: String,
    pub name: String,
    /// Folder of the script that created it.
    pub dir: String,
    pub props: Vec<(String, Vec<String>)>,
}

impl Geometry {
    /// `<dir>/meshes/<name>.<type>`; `None` for types without a mesh file.
    pub fn mesh_path(&self) -> Option<String> {
        let ext = match self.ty.to_ascii_lowercase().as_str() {
            "staticmesh" => "staticmesh",
            "bundledmesh" => "bundledmesh",
            "skinnedmesh" => "skinnedmesh",
            _ => return None,
        };
        Some(format!("{}/meshes/{}.{ext}", self.dir, self.name.to_ascii_lowercase()))
    }
}

/// `Object.create <template>` plus its placement.
#[derive(Clone, Debug)]
pub struct Instance {
    pub template: String,
    pub source: Source,
    pub position: Option<[f32; 3]>,
    /// Yaw/pitch/roll in degrees.
    pub rotation: Option<[f32; 3]>,
    /// Row-vector matrix, translation in row 3 (`Object.absoluteTransformation`).
    pub transform: Option<[[f32; 4]; 4]>,
    pub props: Vec<(String, Vec<String>)>,
}

impl Instance {
    pub fn get_str(&self, method: &str) -> Option<&str> {
        let method = method.to_ascii_lowercase();
        self.props
            .iter()
            .rev()
            .find(|(m, _)| *m == method)
            .and_then(|(_, a)| a.first())
            .map(String::as_str)
    }
}

/// Any command that isn't part of the object model, kept in order.
#[derive(Clone, Debug)]
pub struct Command {
    /// Lowercased `prefix.method`, e.g. `heightmap.setscale`.
    pub name: String,
    pub args: Vec<String>,
    pub source: Source,
}

#[derive(Default, Debug)]
pub struct World {
    /// By lowercased name.
    pub templates: HashMap<String, Template>,
    pub template_order: Vec<String>,
    pub geometries: HashMap<String, Geometry>,
    /// Collision template name → `.collisionmesh` path.
    pub collision_meshes: HashMap<String, String>,
    pub instances: Vec<Instance>,
    pub commands: Vec<Command>,
    active_template: Option<String>,
    active_geometry: Option<String>,
}

impl World {
    pub fn template(&self, name: &str) -> Option<&Template> {
        self.templates.get(&name.to_ascii_lowercase())
    }

    pub fn geometry(&self, name: &str) -> Option<&Geometry> {
        self.geometries.get(&name.to_ascii_lowercase())
    }

    /// Commands with the given lowercased `prefix.method`.
    pub fn commands_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Command> + 'a {
        self.commands.iter().filter(move |c| c.name == name)
    }

    /// First argument of the last command with this name.
    pub fn setting(&self, name: &str) -> Option<&str> {
        self.commands
            .iter()
            .rev()
            .find(|c| c.name == name)
            .and_then(|c| c.args.first())
            .map(String::as_str)
    }

    fn command(&mut self, prefix: &str, method: &str, args: Vec<String>, dir: &str, source: &Source) {
        let prefix = prefix.to_ascii_lowercase();
        let method = method.to_ascii_lowercase();
        match prefix.as_str() {
            "objecttemplate" => self.object_template(&method, args, source),
            "geometrytemplate" => match method.as_str() {
                "create" if args.len() >= 2 => {
                    let key = args[1].to_ascii_lowercase();
                    self.geometries.insert(
                        key.clone(),
                        Geometry {
                            ty: args[0].clone(),
                            name: args[1].clone(),
                            dir: dir.to_string(),
                            props: Vec::new(),
                        },
                    );
                    self.active_geometry = Some(key);
                }
                "active" | "activesafe" if !args.is_empty() => {
                    let key = args[args.len() - 1].to_ascii_lowercase();
                    if self.geometries.contains_key(&key) {
                        self.active_geometry = Some(key);
                    }
                }
                _ => {
                    if let Some(g) = self
                        .active_geometry
                        .as_ref()
                        .and_then(|k| self.geometries.get_mut(k))
                    {
                        g.props.push((method, args));
                    }
                }
            },
            "collisionmanager" if method == "createtemplate" && !args.is_empty() => {
                let name = args[0].to_ascii_lowercase();
                self.collision_meshes
                    .insert(name.clone(), format!("{dir}/meshes/{name}.collisionmesh"));
            }
            "object" => self.object(&method, args, source),
            _ => self.commands.push(Command {
                name: format!("{prefix}.{method}"),
                args,
                source: source.clone(),
            }),
        }
    }

    fn object_template(&mut self, method: &str, args: Vec<String>, source: &Source) {
        match method {
            "create" if args.len() >= 2 => {
                let key = args[1].to_ascii_lowercase();
                if !self.templates.contains_key(&key) {
                    self.template_order.push(key.clone());
                }
                self.templates.insert(key.clone(), new_template(&args[0], &args[1], source));
                self.active_template = Some(key);
            }
            "activesafe" if args.len() >= 2 => {
                let key = args[1].to_ascii_lowercase();
                if !self.templates.contains_key(&key) {
                    self.template_order.push(key.clone());
                    self.templates
                        .insert(key.clone(), new_template(&args[0], &args[1], source));
                }
                self.active_template = Some(key);
            }
            "active" if !args.is_empty() => {
                let key = args[args.len() - 1].to_ascii_lowercase();
                if self.templates.contains_key(&key) {
                    self.active_template = Some(key);
                }
            }
            _ => {
                let Some(t) = self
                    .active_template
                    .as_ref()
                    .and_then(|k| self.templates.get_mut(k))
                else {
                    return;
                };
                match method {
                    "addtemplate" if !args.is_empty() => t.children.push(ChildRef {
                        template: args[0].clone(),
                        position: None,
                        rotation: None,
                    }),
                    "setposition" if !args.is_empty() && !t.children.is_empty() => {
                        t.children.last_mut().expect("checked").position = parse_vec3(&args[0]);
                    }
                    "setrotation" if !args.is_empty() && !t.children.is_empty() => {
                        t.children.last_mut().expect("checked").rotation = parse_vec3(&args[0]);
                    }
                    "geometry" if !args.is_empty() => t.geometry = Some(args[0].clone()),
                    "collisionmesh" | "setcollisionmesh" if !args.is_empty() => {
                        t.collision_mesh = Some(args[0].clone())
                    }
                    _ => t.props.push((method.to_string(), args)),
                }
            }
        }
    }

    fn object(&mut self, method: &str, args: Vec<String>, source: &Source) {
        if method == "create" {
            if let Some(template) = args.first() {
                self.instances.push(Instance {
                    template: template.clone(),
                    source: source.clone(),
                    position: None,
                    rotation: None,
                    transform: None,
                    props: Vec::new(),
                });
            }
            return;
        }
        let Some(instance) = self.instances.last_mut() else {
            return;
        };
        match method {
            "absoluteposition" if !args.is_empty() => instance.position = parse_vec3(&args[0]),
            "rotation" if !args.is_empty() => instance.rotation = parse_vec3(&args[0]),
            "absolutetransformation" if !args.is_empty() => {
                instance.transform = parse_matrix(&args[0]);
                if let Some(m) = instance.transform {
                    instance.position = Some([m[3][0], m[3][1], m[3][2]]);
                }
            }
            _ => instance.props.push((method.to_string(), args)),
        }
    }
}

fn new_template(ty: &str, name: &str, source: &Source) -> Template {
    Template {
        ty: ty.to_string(),
        name: name.to_string(),
        source: source.clone(),
        props: Vec::new(),
        children: Vec::new(),
        geometry: None,
        collision_mesh: None,
    }
}

// ---------------------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Scope {
    vars: HashMap<String, String>,
}

impl Scope {
    fn with_args(args: &[String]) -> Self {
        let mut vars = HashMap::new();
        for i in 1..=8 {
            vars.insert(format!("v_arg{i}"), args.get(i - 1).cloned().unwrap_or_default());
        }
        vars.insert("v_result".into(), String::new());
        Self { vars }
    }
}

/// Runs scripts from a [`Vfs`] into a [`World`].
pub struct Interpreter<'a> {
    vfs: &'a Vfs,
    pub world: World,
    /// Resolve `Object.create foo` by loading `objects/**/foo.con` on demand, like the game.
    pub lazy_templates: bool,
    consts: HashMap<String, String>,
    allow_multiple_load: bool,
    loaded: HashSet<String>,
    parsed: HashMap<String, Arc<Vec<Node>>>,
    template_index: Option<HashMap<String, String>>,
    /// Templates whose children [`Self::ensure_template`] has loaded.
    children_loaded: HashSet<String>,
    depth: u32,
    pub missing_files: BTreeMap<String, u32>,
    pub missing_templates: BTreeMap<String, u32>,
    pub warnings: Vec<String>,
}

const MAX_DEPTH: u32 = 64;

impl<'a> Interpreter<'a> {
    pub fn new(vfs: &'a Vfs) -> Self {
        Self {
            vfs,
            world: World::default(),
            lazy_templates: true,
            consts: HashMap::new(),
            allow_multiple_load: true,
            loaded: HashSet::new(),
            parsed: HashMap::new(),
            template_index: None,
            children_loaded: HashSet::new(),
            depth: 0,
            missing_files: BTreeMap::new(),
            missing_templates: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    /// `run <path> args...`: executes a file in a new scope.
    pub fn run(&mut self, path: &str, args: &[String]) {
        let mut scope = Scope::with_args(args);
        self.exec_file(&normalize(path), &mut scope);
    }

    /// Makes sure a template is defined, loading `<name>.con` (or `<name>_compiled.con`
    /// for compiled roads) from anywhere under `objects/` if needed, and so are the child
    /// templates it adds (`ObjectTemplate.addTemplate`): the game loads those on demand too,
    /// e.g. the carrier's ladders, which live in files of their own that only the editor's
    /// part of `StaticObjects.con` runs.
    pub fn ensure_template(&mut self, name: &str) {
        let key = name.to_ascii_lowercase();
        if !self.lazy_templates {
            return;
        }
        self.load_template(&key);
        self.ensure_children(&key, 0);
    }

    fn ensure_children(&mut self, key: &str, depth: u32) {
        let Some(template) = self.world.templates.get(key) else {
            return;
        };
        if depth > 16 || template.children.is_empty() || !self.children_loaded.insert(key.to_string()) {
            return;
        }
        let children: Vec<String> = template.children.iter().map(|c| c.template.to_ascii_lowercase()).collect();
        for child in children {
            self.load_template(&child);
            self.ensure_children(&child, depth + 1);
        }
    }

    fn load_template(&mut self, key: &str) {
        if self.world.templates.contains_key(key) {
            return;
        }
        let key = key.to_string();
        let vfs = self.vfs;
        let index = self.template_index.get_or_insert_with(|| build_template_index(vfs));
        let paths: Vec<String> = [key.clone(), format!("{key}_compiled")]
            .iter()
            .filter_map(|candidate| index.get(candidate).cloned())
            .collect();
        for path in paths {
            self.run(&path, &[]);
            if self.world.templates.contains_key(&key) {
                return;
            }
        }
        *self.missing_templates.entry(key).or_default() += 1;
    }

    fn resolve(&self, path: &str, dir: &str) -> Option<String> {
        let path = path.trim_matches('"').replace('\\', "/");
        let full = if path.starts_with('/') {
            normalize(&path)
        } else {
            normalize(&format!("{dir}/{path}"))
        };
        [full.clone(), format!("{full}.con")]
            .into_iter()
            .find(|candidate| self.vfs.exists(candidate))
    }

    fn exec_file(&mut self, path: &str, scope: &mut Scope) {
        if !self.allow_multiple_load && self.loaded.contains(path) {
            return;
        }
        if self.depth >= MAX_DEPTH {
            self.warnings.push(format!("maximum script depth reached at {path}"));
            return;
        }
        let tree = match self.parsed.get(path) {
            Some(tree) => tree.clone(),
            None => {
                let Ok(text) = self.vfs.read_text(path) else {
                    *self.missing_files.entry(path.to_string()).or_default() += 1;
                    return;
                };
                let mut warnings = Vec::new();
                let tree = Arc::new(parse(lex(&text), &mut warnings));
                self.warnings
                    .extend(warnings.into_iter().map(|w| format!("{path}: {w}")));
                self.parsed.insert(path.to_string(), tree.clone());
                tree
            }
        };
        self.loaded.insert(path.to_string());
        let dir = path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        let file: Source = Arc::from(path);
        self.depth += 1;
        self.exec_block(&tree, scope, &dir, &file);
        self.depth -= 1;
    }

    fn exec_block(&mut self, nodes: &[Node], scope: &mut Scope, dir: &str, file: &Source) {
        for node in nodes {
            match node {
                Node::Stmt(stmt) => self.exec_stmt(stmt, scope, dir, file),
                Node::If {
                    branches,
                    else_body,
                    ..
                } => {
                    let taken = branches.iter().find(|(cond, _)| self.eval_cond(cond, scope));
                    match (taken, else_body) {
                        (Some((_, body)), _) => self.exec_block(body, scope, dir, file),
                        (None, Some(body)) => self.exec_block(body, scope, dir, file),
                        (None, None) => {}
                    }
                }
                Node::While { cond, body, .. } => {
                    let mut guard = 0;
                    while guard < 100_000 && self.eval_cond(cond, scope) {
                        self.exec_block(body, scope, dir, file);
                        guard += 1;
                    }
                }
            }
        }
    }

    fn subst(&self, token: &Token, scope: &Scope) -> String {
        if token.quoted {
            return token.text.clone();
        }
        let lower = token.text.to_ascii_lowercase();
        if lower.starts_with("v_") {
            if let Some(v) = scope.vars.get(&lower) {
                return v.clone();
            }
        } else if lower.starts_with("c_")
            && let Some(v) = self.consts.get(&lower)
        {
            return v.clone();
        }
        token.text.clone()
    }

    fn eval_cond(&self, tokens: &[Token], scope: &Scope) -> bool {
        let values: Vec<String> = tokens.iter().map(|t| self.subst(t, scope)).collect();
        match values.as_slice() {
            [] => false,
            [single] => !single.is_empty() && single != "0",
            [a, op, rest @ ..] if !rest.is_empty() => {
                let b = rest.join(" ");
                let ordering = match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
                    (Ok(x), Ok(y)) => x.partial_cmp(&y),
                    _ => Some(a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())),
                };
                let Some(ordering) = ordering else {
                    return false;
                };
                use std::cmp::Ordering::*;
                match op.to_ascii_lowercase().as_str() {
                    "==" | "equals" => ordering == Equal,
                    "!=" | "notequals" => ordering != Equal,
                    ">" | "greaterthan" => ordering == Greater,
                    ">=" | "greaterorequalthan" => ordering != Less,
                    "<" | "lessthan" => ordering == Less,
                    "<=" | "lessorequalthan" => ordering != Greater,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn exec_stmt(&mut self, stmt: &Stmt, scope: &mut Scope, dir: &str, file: &Source) {
        let head = &stmt.tokens[0].text;
        let keyword = head.to_ascii_lowercase();
        let args: Vec<String> = stmt.tokens[1..].iter().map(|t| self.subst(t, scope)).collect();
        let source = || -> Source { Arc::from(format!("{file}:{}", stmt.line)) };

        match keyword.as_str() {
            "run" | "include" => {
                let Some(target) = stmt.tokens.get(1) else {
                    return;
                };
                let Some(path) = self.resolve(&target.text, dir) else {
                    let missing = if target.text.starts_with(['/', '\\']) {
                        normalize(&target.text)
                    } else {
                        normalize(&format!("{dir}/{}", target.text))
                    };
                    *self.missing_files.entry(missing).or_default() += 1;
                    return;
                };
                if keyword == "run" {
                    let mut inner = Scope::with_args(&args[1..]);
                    self.exec_file(&path, &mut inner);
                } else {
                    for (i, arg) in args[1..].iter().take(8).enumerate() {
                        scope.vars.insert(format!("v_arg{}", i + 1), arg.clone());
                    }
                    self.exec_file(&path, scope);
                }
            }
            "var" | "const" => {
                let Some(name) = stmt.tokens.get(1) else {
                    return;
                };
                let name = name.text.to_ascii_lowercase();
                let value = if stmt.tokens.len() >= 4 && stmt.tokens[2].text == "=" {
                    args[2..].join(" ")
                } else {
                    args.get(1).cloned().unwrap_or_default()
                };
                if keyword == "const" {
                    self.consts.insert(name, value);
                } else {
                    scope.vars.insert(name, value);
                }
            }
            "return" | "echo" | "alias" => {}
            "console.allowmultiplefileload" => {
                self.allow_multiple_load = args.first().is_none_or(|a| a != "0");
            }
            _ => {
                let Some((prefix, method)) = head.split_once('.') else {
                    return;
                };
                if prefix.eq_ignore_ascii_case("object")
                    && method.eq_ignore_ascii_case("create")
                    && let Some(name) = args.first()
                {
                    self.ensure_template(name);
                }
                self.world.command(prefix, method, args, dir, &source());
                if let Some(var) = &stmt.assign {
                    scope.vars.insert(var.to_ascii_lowercase(), String::new());
                }
            }
        }
    }
}

/// Basename (lowercase, without `.con`) → path, for every `.con` under `objects/`.
fn build_template_index(vfs: &Vfs) -> HashMap<String, String> {
    let mut index = HashMap::new();
    let mut paths: Vec<&str> = vfs
        .list("objects")
        .filter(|p| p.ends_with(".con"))
        .collect();
    // Deterministic choice when a basename exists more than once.
    paths.sort_unstable();
    for path in paths {
        let base = path.rsplit('/').next().unwrap_or(path);
        let base = base.strip_suffix(".con").unwrap_or(base);
        index.entry(base.to_string()).or_insert_with(|| path.to_string());
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ascii_whitespace_separates() {
        let tokens = tokenize_line("Object.create Pagoda\u{a0}_1");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["Object.create", "Pagoda\u{a0}_1"]);
    }

    #[test]
    fn tokenizes_quotes() {
        let t = tokenize_line("  Foo.bar \"a b\" c/d/e  x=\"y z\"");
        let texts: Vec<_> = t.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["Foo.bar", "a b", "c/d/e", "x=\"y z\""]);
        assert!(t[1].quoted);
    }

    #[test]
    fn lexes_comments() {
        let text = "rem hello\nbeginRem\nObject.create a\nendRem\nObject.create b\r\n";
        let stmts = lex(text);
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].tokens[1].text, "b");
    }

    #[test]
    fn parses_if_else() {
        let text = "if v_arg1 == host\nA.a\nelse\nB.b\nendIf\nC.c\n";
        let mut warnings = Vec::new();
        let nodes = parse(lex(text), &mut warnings);
        assert!(warnings.is_empty());
        assert_eq!(nodes.len(), 2);
        match &nodes[0] {
            Node::If {
                branches,
                else_body,
                ..
            } => {
                assert_eq!(branches[0].1.len(), 1);
                assert_eq!(else_body.as_ref().unwrap().len(), 1);
            }
            _ => panic!("expected if"),
        }
    }

    #[test]
    fn parses_matrices() {
        let m = parse_matrix("[1/0/0/0][0/1/0/0][0/0/1/0][5/6/7/1]").unwrap();
        assert_eq!(m[3], [5.0, 6.0, 7.0, 1.0]);
    }
}
