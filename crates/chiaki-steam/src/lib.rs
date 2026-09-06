//! Steam-Library-Shortcut-Verwaltung (Windows-only).
//!
//! 1:1-Port von `third-party/cpp-steam-tools` (Referenz: chiaki-ng) plus der
//! Parameter-Logik aus `gui/src/qmlbackend.cpp::createSteamShortcut`
//! ("Add to Steam library").
//!
//! Quell-Zuordnung:
//! - `crc.h`                     → [`crc32`] (Tabelle per `const fn` statt generiertem Array)
//! - `steamtools.cpp`            → ID-Generierung, shortcuts.vdf read/write, Backup, Grid-Images
//! - `vdfstatemachine.cpp`       → [`ShortcutVdfStateMachine`] (binäres shortcuts.vdf-Format)
//! - `vdf_parser.hpp` (tyti)     → [`parse_vdf`]/[`write_vdf`] (Text-VDF, für Controller-Configs
//!                                 und `libraryfolders.vdf`)
//! - `qmlbackend.cpp`            → [`SteamShortcuts::add_to_library`], Controller-Workshop-ID,
//!                                 Exe/StartDir/Tags-Parameter
//!
//! Abweichungen vom C++-Stand (dokumentiert):
//! - Steam-Pfad: C++ hartkodiert `C:/Program Files (x86)/Steam`; hier zusätzlich
//!   Registry-Lookup `HKCU\Software\Valve\Steam\SteamPath` (expliziter Pfad > Registry > Fallback).
//! - `#include`/`#base` im Text-VDF werden ignoriert (keine Datei-Einbindung), statt sie
//!   nachzuladen — für libraryfolders.vdf und Controller-Configs irrelevant.
//! - Backup-Zeitstempel in UTC statt Lokalzeit (keine TZ-Abhängigkeit in std).
//! - Fehlendes `shortcuts.vdf` ergibt eine leere Liste (C++: Fehler-Callback + leere Liste);
//!   erster Shortcut wird damit sauber angelegt.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Fehler
// ---------------------------------------------------------------------------

/// Fehler der Steam-Shortcut-Verwaltung.
#[derive(Debug, thiserror::Error)]
pub enum SteamError {
    #[error("Steam installation not found: {path:?}")]
    SteamNotFound { path: PathBuf },

    #[error("no Steam user found in {path:?} (loginusers.vdf missing or no most-recent user)")]
    NoSteamUser { path: PathBuf },

    #[error("io error for {path:?}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("io error: {source}")]
    IoOther { source: std::io::Error },

    #[error("VDF parse error: {0}")]
    VdfParse(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

// ---------------------------------------------------------------------------
// CRC32 (Port von crc.h — pycrc: Poly 0x04c11db7, reflected, init/xorout 0xffffffff)
// ---------------------------------------------------------------------------

/// CRC-Tabelle, per `const fn` statt generiertem statischem Array (crc.h).
const fn make_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = make_crc_table();

/// Standard-CRC-32 (IEEE), identisch zu `crc_init()/crc_update()/crc_finalize()` aus crc.h
/// und zu `zlib.crc32`.
pub fn crc32(data: &[u8]) -> u32 {
    // crc_init() == 0xffffffff
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = CRC_TABLE[idx] ^ (crc >> 8);
    }
    // crc_finalize() == ^0xffffffff
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Shortcut-/App-ID-Generierung (steamtools.cpp, generatePreliminaryId et al.)
// ---------------------------------------------------------------------------

/// `generatePreliminaryId(exe, appname)`: CRC32 über `exe + appname` (UTF-8-Bytes,
/// analog `strlen(key.c_str())`), High-Bit gesetzt, kombiniert mit der Fixed-Points-32.
/// `exe` enthält hier die Anführungszeichen, wie im C++ (`"\"" + filepath + "\""`).
fn generate_preliminary_id(exe: &str, appname: &str) -> u64 {
    let key = format!("{exe}{appname}");
    let crc = crc32(key.as_bytes());
    let top = (crc as u64) | 0x8000_0000;
    (top << 32) | 0x0200_0000
}

/// `generateAppId`: vollständige preliminäre ID als Dezimalstring.
pub fn generate_app_id(exe: &str, appname: &str) -> String {
    generate_preliminary_id(exe, appname).to_string()
}

/// `generateShortAppId`: obere 32 Bit der preliminären ID (wird für die
/// Grid-Image-Dateinamen verwendet).
pub fn generate_short_app_id(exe: &str, appname: &str) -> String {
    (generate_preliminary_id(exe, appname) >> 32).to_string()
}

/// `generateShortcutId`: `(preliminaryId >> 32) - 0x100000000` als u32 —
/// identisch zu `(crc | 0x80000000) as u32` (Wrap-around, wie im C++-Cast).
pub fn generate_shortcut_id(exe: &str, appname: &str) -> u32 {
    let preliminary_id = generate_preliminary_id(exe, appname);
    ((preliminary_id >> 32).wrapping_sub(0x1_0000_0000)) as u32
}

// ---------------------------------------------------------------------------
// Text-VDF (Port von vdf_parser.hpp, tyti::vdf) — für libraryfolders.vdf und
// die Controller-Configs.
// ---------------------------------------------------------------------------

/// Ein Knoten des Text-VDF: Name, Attribute (Key/Value) und Kind-Objekte.
/// `BTreeMap` statt `std::unordered_map` — deterministische (sortierte) Reihenfolge
/// beim Schreiben; der C++-Writer hatte ohnehin unbestimmte Reihenfolge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VdfObject {
    pub name: String,
    pub attribs: BTreeMap<String, String>,
    pub childs: BTreeMap<String, VdfObject>,
}

impl VdfObject {
    /// `add_attribute` (tyti): Attribut setzen.
    pub fn add_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attribs.insert(key.into(), value.into());
    }

    /// `add_child` (tyti): Kindobjekt nach Namen einhängen.
    pub fn add_child(&mut self, child: VdfObject) {
        self.childs.insert(child.name.clone(), child);
    }
}

const VDF_WHITESPACES: [u8; 6] = [b' ', b'\n', 0x0B, 0x0C, b'\r', b'\t'];

fn is_vdf_whitespace(c: u8) -> bool {
    VDF_WHITESPACES.contains(&c)
}

fn skip_vdf_whitespaces(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && is_vdf_whitespace(bytes[i]) {
        i += 1;
    }
    i
}

fn find_byte_from(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes[from..].iter().position(|&b| b == needle).map(|p| p + from)
}

fn find_first_vdf_whitespace_from(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..].iter().position(|&b| is_vdf_whitespace(b)).map(|p| p + from)
}

fn find_sub_from(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Kommentar überspringen (`//`-Zeile oder `/*...*/`-Block); 1:1 wie `skip_comments`
/// im tyti-Parser (Zeilenkommentar endet AUF dem `\n`).
fn skip_vdf_comments(bytes: &[u8], pos: usize) -> usize {
    let n = bytes.len();
    let mut iter = pos + 1; // hinter dem ersten '/'
    if iter >= n {
        return n;
    }
    if bytes[iter] == b'/' {
        // Zeilenkommentar: bis zum '\n' suchen (Position des '\n' zurückgeben)
        iter = match find_byte_from(bytes, iter + 1, b'\n') {
            Some(p) => p,
            None => return n,
        };
    }
    if bytes[iter] == b'*' {
        // Blockkommentar: bis zum nächsten "*/"
        return match find_sub_from(bytes, iter + 1, b"*/") {
            // C++: distance(iter, last) <= 2 → last
            Some(p) if n - p <= 2 => n,
            Some(p) => p + 2,
            None => n,
        };
    }
    iter
}

/// Ende eines quoted Strings finden: schließendes `"` nur, wenn davor eine
/// gerade Anzahl Backslashes steht (`end_quote` aus vdf_parser.hpp).
fn vdf_end_quote(bytes: &[u8], begin: usize) -> Result<usize, SteamError> {
    let n = bytes.len();
    if begin >= n {
        return Err(SteamError::VdfParse("quote was opened but not closed.".into()));
    }
    let mut iter = begin;
    loop {
        iter += 1;
        iter = match find_byte_from(bytes, iter, b'"') {
            Some(p) => p,
            None => break,
        };
        // Backslash-Lauf direkt vor dem Quote zurückgehen; Abstand gerade → terminierend
        let mut last_esc = iter - 1;
        while last_esc != begin && bytes[last_esc] == b'\\' {
            last_esc -= 1;
        }
        let distance = iter - last_esc;
        if distance % 2 == 1 {
            return Ok(iter);
        }
    }
    Err(SteamError::VdfParse("quote was opened but not closed.".into()))
}

/// Ende eines unquoted Worts finden (erstes Whitespace mit gerader
/// Backslash-Anzahl davor); `end_word` aus vdf_parser.hpp.
fn vdf_end_word(bytes: &[u8], begin: usize) -> Result<usize, SteamError> {
    let n = bytes.len();
    if begin >= n {
        // Meldung 1:1 aus tyti übernommen
        return Err(SteamError::VdfParse("quote was opened but not closed.".into()));
    }
    let mut iter = begin;
    loop {
        iter += 1;
        iter = match find_first_vdf_whitespace_from(bytes, iter) {
            Some(p) => p,
            None => break,
        };
        let mut last_esc = iter - 1;
        while last_esc != begin && bytes[last_esc] == b'\\' {
            last_esc -= 1;
        }
        let distance = iter - last_esc;
        if distance % 2 == 1 {
            return Ok(iter);
        }
    }
    Err(SteamError::VdfParse("word wasnt properly ended".into()))
}

/// `strip_escape_symbols` (tyti): `\"` → `"` und `\\` → `\` (zwei Durchläufe, wie im C++).
fn strip_vdf_escape_symbols(s: &mut String) {
    *s = s.replace("\\\"", "\"");
    *s = s.replace("\\\\", "\\");
}

/// Plattform-Konditional `[...]` auswerten (WIN32-Build: `$WIN32`/`$WINDOWS` erfüllt,
/// `!` negiert); `conditional_fullfilled` aus vdf_parser.hpp.
fn vdf_conditional_fullfilled(bytes: &[u8], i: &mut usize) -> Result<bool, SteamError> {
    let n = bytes.len();
    *i = skip_vdf_whitespaces(bytes, *i);
    if *i >= n {
        return Ok(true);
    }
    if bytes[*i] == b'[' {
        *i += 1;
        if *i >= n {
            return Err(SteamError::VdfParse("conditional not closed".into()));
        }
        let end = match find_byte_from(bytes, *i, b']') {
            Some(p) => p,
            None => return Err(SteamError::VdfParse("conditional not closed".into())),
        };
        let mut start = *i;
        let negate = bytes[start] == b'!';
        if negate {
            start += 1;
        }
        let conditional = String::from_utf8_lossy(&bytes[start..end]).into_owned();
        *i = (end + 1).min(n);
        let is_platform = conditional == "$WIN32" || conditional == "$WINDOWS";
        return Ok(is_platform != negate);
    }
    Ok(true)
}

/// Text-VDF parsen (Port von `tyti::vdf::read`, WIN32-Variante).
///
/// Unterstützt: quoted/unquoted Keys+Values, `\"`/`\\`-Escapes, `//`- und
/// `/* */`-Kommentare, `[...]`-Plattform-Konditionale, verschachtelte Objekte,
/// mehrere Roots (werden wie in tyti unter einem namenlosen Root zusammengefasst).
/// `#include`/`#base` werden erkannt, aber ignoriert (keine Datei-Einbindung).
pub fn parse_vdf(input: &[u8]) -> Result<VdfObject, SteamError> {
    let n = input.len();
    let mut roots: Vec<VdfObject> = Vec::new();
    let mut stack: Vec<VdfObject> = Vec::new();
    let mut cur: Option<VdfObject> = None;
    let mut i = 0usize;

    while i < n && input[i] != 0 {
        i = skip_vdf_whitespaces(input, i);
        if i >= n || input[i] == 0 {
            break;
        }
        if input[i] == b'/' {
            i = skip_vdf_comments(input, i);
            if i >= n || input[i] == 0 {
                return Err(SteamError::VdfParse("Unexpected eof".into()));
            }
        } else if input[i] != b'}' {
            // --- Key lesen ---
            let quoted = input[i] == b'"';
            let key_end = if quoted {
                vdf_end_quote(input, i)?
            } else {
                vdf_end_word(input, i)?
            };
            let start = if quoted { i + 1 } else { i };
            let mut key = String::from_utf8_lossy(&input[start..key_end]).into_owned();
            strip_vdf_escape_symbols(&mut key);
            i = key_end + if input[key_end] == b'"' { 1 } else { 0 };
            if i >= n {
                return Err(SteamError::VdfParse("key opened, but never closed".into()));
            }

            i = skip_vdf_whitespaces(input, i);

            if !vdf_conditional_fullfilled(input, &mut i)? {
                continue; // Konditional nicht erfüllt → Paar überspringen
            }
            if i >= n {
                return Err(SteamError::VdfParse("key declared, but no value".into()));
            }

            // Kommentare zwischen Key und Value überspringen
            while i < n && input[i] == b'/' {
                i = skip_vdf_comments(input, i);
                if i >= n || input[i] == b'}' {
                    return Err(SteamError::VdfParse("key declared, but no value".into()));
                }
                i = skip_vdf_whitespaces(input, i);
                if i >= n || input[i] == b'}' {
                    return Err(SteamError::VdfParse("key declared, but no value".into()));
                }
            }

            // --- Value oder Child-Objekt ---
            if i < n && input[i] != b'{' {
                let vquoted = input[i] == b'"';
                let value_end = if vquoted {
                    vdf_end_quote(input, i)?
                } else {
                    vdf_end_word(input, i)?
                };
                if value_end >= n {
                    return Err(SteamError::VdfParse("No closed word".into()));
                }
                let vstart = if vquoted { i + 1 } else { i };
                if vstart >= n {
                    return Err(SteamError::VdfParse("No closed word".into()));
                }
                let mut value = String::from_utf8_lossy(&input[vstart..value_end]).into_owned();
                strip_vdf_escape_symbols(&mut value);
                i = value_end + if input[value_end] == b'"' { 1 } else { 0 };
                if i >= n {
                    return Err(SteamError::VdfParse("No closed word".into()));
                }
                if !vdf_conditional_fullfilled(input, &mut i)? {
                    continue;
                }
                if key != "#include" && key != "#base" {
                    match cur.as_mut() {
                        Some(obj) => {
                            obj.attribs.insert(key, value);
                        }
                        None => {
                            return Err(SteamError::VdfParse("unexpected key without object".into()));
                        }
                    }
                } else {
                    // Datei-Einbindung nicht unterstützt → ignoriert (siehe Modul-Doku)
                    debug!("vdf: ignoring #include/#base for '{value}'");
                }
            } else {
                // Child-Objekt aufmachen
                if let Some(prev) = cur.take() {
                    stack.push(prev);
                }
                cur = Some(VdfObject {
                    name: key,
                    attribs: BTreeMap::new(),
                    childs: BTreeMap::new(),
                });
                i += 1;
            }
        } else {
            // '}' — Objekt schließen
            match cur.take() {
                Some(obj) => {
                    if let Some(mut prev) = stack.pop() {
                        prev.add_child(obj);
                        cur = Some(prev);
                    } else {
                        roots.push(obj);
                    }
                    i += 1;
                }
                None => return Err(SteamError::VdfParse("unexpected '}'".into())),
            }
        }
    }

    if cur.is_some() || !stack.is_empty() {
        return Err(SteamError::VdfParse("object is not closed with '}'".into()));
    }

    // tyti read(): mehrere Roots → namenloser Wrapper; einer → dieser; keiner → leer
    let mut result = VdfObject::default();
    if roots.len() > 1 {
        for r in roots {
            result.add_child(r);
        }
    } else if let Some(single) = roots.into_iter().next() {
        result = single;
    }
    Ok(result)
}

/// Text-VDF schreiben (Port von `tyti::vdf::write`): tabelliert mit Tabs,
/// Attribute als `"key"\t\t"value"`.
pub fn write_vdf(obj: &VdfObject) -> String {
    let mut out = String::new();
    write_vdf_rec(&mut out, obj, 0);
    out
}

fn write_vdf_rec(out: &mut String, obj: &VdfObject, depth: usize) {
    let tab = "\t".repeat(depth);
    out.push_str(&tab);
    out.push('"');
    out.push_str(&obj.name);
    out.push_str("\"\n");
    out.push_str(&tab);
    out.push_str("{\n");
    for (k, v) in &obj.attribs {
        out.push_str(&"\t".repeat(depth + 1));
        out.push('"');
        out.push_str(k);
        out.push_str("\"\t\t\"");
        out.push_str(v);
        out.push_str("\"\n");
    }
    for child in obj.childs.values() {
        write_vdf_rec(out, child, depth + 1);
    }
    out.push_str(&tab);
    out.push_str("}\n");
}

// ---------------------------------------------------------------------------
// Binäres shortcuts.vdf (Port von vdfstatemachine.cpp + writeShortcutAttribute)
// ---------------------------------------------------------------------------

/// Header des Shortcut-Files, dient der Gültigkeitsprüfung.
const SHORTCUT_HEADER: [u8; 11] = [0x00, b's', b'h', b'o', b'r', b't', b'c', b'u', b't', b's', 0x00];

/// Feldtyp eines Shortcut-Attributs (`FieldType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Boolean,
    List,
    Date,
    AppId,
}

/// Ein Shortcut-Attribut (`steam_shortcut_property`): Wert als String wie im C++
/// (Booleans als "true"/"false", Listen kommagetrennt, App-ID als Dezimalstring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutProperty {
    pub value: String,
    pub field_type: FieldType,
}

impl ShortcutProperty {
    pub fn new(field_type: FieldType, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            field_type,
        }
    }
}

/// Ein Non-Steam-Shortcut aus shortcuts.vdf (`SteamShortcutEntry`).
/// Properties sind wie in der C++-`QMap` nach Lowercase-Key sortiert;
/// `keys` bildet Lowercase-Key → Original-Key (z. B. "appname" → "AppName").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteamShortcutEntry {
    properties: BTreeMap<String, ShortcutProperty>,
    keys: BTreeMap<String, String>,
}

impl SteamShortcutEntry {
    /// `setProperty(key, real_key, property)`.
    pub fn set_property(&mut self, key: impl Into<String>, real_key: impl Into<String>, property: ShortcutProperty) {
        let key = key.into();
        self.properties.insert(key.clone(), property);
        self.keys.insert(key, real_key.into());
    }

    /// Attribut nach Lowercase-Key (`properties.value(key).value`).
    pub fn property(&self, key: &str) -> Option<&ShortcutProperty> {
        self.properties.get(key)
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(|p| p.value.as_str())
    }

    pub fn app_id(&self) -> Option<&str> {
        self.get("appid")
    }

    pub fn app_name(&self) -> Option<&str> {
        self.get("appname")
    }

    /// Exe inkl. Anführungszeichen, wie sie in der Datei steht (C++ `getExe()`).
    pub fn exe(&self) -> Option<&str> {
        self.get("exe")
    }

    /// Exe ohne umschließende Anführungszeichen.
    pub fn exe_unquoted(&self) -> Option<&str> {
        self.get("exe")
            .and_then(|v| v.strip_prefix('"'))
            .and_then(|v| v.strip_suffix('"'))
    }

    pub fn start_dir(&self) -> Option<&str> {
        self.get("startdir")
    }

    pub fn icon(&self) -> Option<&str> {
        self.get("icon")
    }

    pub fn shortcut_path(&self) -> Option<&str> {
        self.get("shortcutpath")
    }

    pub fn launch_options(&self) -> Option<&str> {
        self.get("launchoptions")
    }

    pub fn last_play_time(&self) -> Option<&str> {
        self.get("lastplaytime")
    }

    /// Tags als Liste (in der Datei kommagetrennt).
    pub fn tags(&self) -> Vec<&str> {
        self.get("tags").map(|v| v.split(',').collect()).unwrap_or_default()
    }
}

/// Parse-States (`VDFStateMachine::ParseState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    EntryId,
    Waiting,
    Key,
    Value,
    Ending,
}

/// Listen-Sub-States (`VDFStateMachine::ListParseState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListParseState {
    Waiting,
    Index,
    Value,
    /// Im C++ vorhanden, wird aber nie betreten — 1:1 übernommen.
    #[allow(dead_code)]
    Ending,
}

/// Byte-weiser State-Machine-Parser für shortcuts.vdf
/// (1:1-Port der Namespaces `ENTRYID`/`WAITING`/`KEY`/`VALUE`/`ENDING`).
#[derive(Debug)]
struct ShortcutVdfStateMachine {
    state: ParseState,
    list_state: ListParseState,
    field_type: FieldType,
    utf8_string: Vec<u8>,
    key: String,
    entry: SteamShortcutEntry,
    list_value: Vec<String>,
    appid_bytes: Vec<u8>,
    ending_buffer: Vec<u8>,
    shortcuts: Vec<SteamShortcutEntry>,
}

impl ShortcutVdfStateMachine {
    fn new() -> Self {
        Self {
            state: ParseState::EntryId,
            list_state: ListParseState::Waiting,
            field_type: FieldType::String,
            utf8_string: Vec::new(),
            key: String::new(),
            entry: SteamShortcutEntry::default(),
            list_value: Vec::new(),
            appid_bytes: Vec::new(),
            ending_buffer: Vec::new(),
            shortcuts: Vec::new(),
        }
    }

    fn handle_byte(&mut self, value: u8) {
        match self.state {
            ParseState::EntryId => self.handle_entry_id(value),
            ParseState::Waiting => self.handle_waiting(value),
            ParseState::Key => self.handle_key(value),
            ParseState::Value => self.handle_value(value),
            ParseState::Ending => self.handle_ending(value),
        }
    }

    /// `ENTRYID::handleState`: wartet auf den Typ-Byte eines Eintrags-Attributs
    /// (Entry-Präfix `\x00<id>\x00` wird ignoriert).
    fn handle_entry_id(&mut self, value: u8) {
        match value {
            0x02 => {
                self.state = ParseState::Key;
                self.field_type = FieldType::Boolean;
            }
            0x01 => {
                self.state = ParseState::Key;
                self.field_type = FieldType::String;
            }
            _ => {}
        }
    }

    /// `WAITING::handleState`: nächster Attribut-Typ (oder Listen-/Dateiende).
    fn handle_waiting(&mut self, value: u8) {
        if value != 0x08 {
            self.state = ParseState::Key;
            match value {
                0x01 => self.field_type = FieldType::String,
                0x02 => self.field_type = FieldType::Boolean,
                0x00 => {
                    self.list_value.clear();
                    self.field_type = FieldType::List;
                }
                _ => {}
            }
        }
    }

    /// `KEY::handleState`: Key bis zum NUL akkumulieren; appid/lastplaytime
    /// bekommen ihre Typen erzwungen.
    fn handle_key(&mut self, value: u8) {
        if value != 0x00 {
            self.utf8_string.push(value);
            // C++-Quirk: enthält der akkumulierte Key ein 0x01-Byte, Puffer verwerfen
            if self.utf8_string.contains(&0x01) {
                self.utf8_string.clear();
            }
        } else {
            self.key = String::from_utf8_lossy(&self.utf8_string).into_owned();
            self.utf8_string.clear();
            self.state = ParseState::Value;
            if self.key == "lastplaytime" {
                self.field_type = FieldType::Date;
            }
            if self.key == "appid" {
                self.field_type = FieldType::AppId;
            }
        }
    }

    /// `VALUE::handleState`.
    fn handle_value(&mut self, value: u8) {
        match self.field_type {
            FieldType::String => {
                if value != 0x00 {
                    self.utf8_string.push(value);
                } else {
                    let value = String::from_utf8_lossy(&self.utf8_string).into_owned();
                    self.entry.set_property(
                        self.key.to_lowercase(),
                        self.key.clone(),
                        ShortcutProperty {
                            value,
                            field_type: self.field_type,
                        },
                    );
                    self.utf8_string.clear();
                    self.state = ParseState::Waiting;
                }
            }
            FieldType::Boolean => {
                let bool_value = match value {
                    0x01 => "true",
                    0x00 => "false",
                    _ => "",
                };
                if !bool_value.is_empty() {
                    self.entry.set_property(
                        self.key.to_lowercase(),
                        self.key.clone(),
                        ShortcutProperty {
                            value: bool_value.to_string(),
                            field_type: self.field_type,
                        },
                    );
                }
                self.state = ParseState::Ending;
            }
            FieldType::Date => {
                // Datum-Inhalt wird (wie im C++) verworfen
                self.entry.set_property(
                    self.key.to_lowercase(),
                    self.key.clone(),
                    ShortcutProperty {
                        value: String::new(),
                        field_type: self.field_type,
                    },
                );
                self.state = ParseState::Ending;
            }
            FieldType::AppId => {
                // C++: *reinterpret_cast<const uint32_t*>(bytes) — little-endian (x86)
                if self.appid_bytes.len() < 4 {
                    self.appid_bytes.push(value);
                }
                if self.appid_bytes.len() >= 4 {
                    let id = u32::from_le_bytes([
                        self.appid_bytes[0],
                        self.appid_bytes[1],
                        self.appid_bytes[2],
                        self.appid_bytes[3],
                    ]);
                    self.entry.set_property(
                        self.key.to_lowercase(),
                        self.key.clone(),
                        ShortcutProperty {
                            value: id.to_string(),
                            field_type: self.field_type,
                        },
                    );
                    self.appid_bytes.clear();
                    self.state = ParseState::Waiting;
                }
            }
            FieldType::List => self.handle_list_value(value),
        }
    }

    /// LIST-Zweig von `VALUE::handleState`.
    fn handle_list_value(&mut self, value: u8) {
        if self.list_state == ListParseState::Waiting && value == 0x08 && self.ending_buffer.is_empty() {
            self.ending_buffer.push(value);
        } else if self.list_state == ListParseState::Waiting && value == 0x08 && !self.ending_buffer.is_empty() {
            let joined = self.list_value.join(",");
            self.entry.set_property(
                self.key.to_lowercase(),
                self.key.clone(),
                ShortcutProperty {
                    value: joined,
                    field_type: self.field_type,
                },
            );
            self.ending_buffer.clear();
            // C++: "tags ist die einzige Liste und der letzte Block" — nach dem
            // Listen-Entry-Ende ist der Shortcut komplett.
            self.list_state = ListParseState::Waiting;
            self.state = ParseState::EntryId;
            let entry = std::mem::take(&mut self.entry);
            self.shortcuts.push(entry);
        } else if self.list_state == ListParseState::Waiting && value != 0x08 {
            self.list_state = ListParseState::Index;
        } else if self.list_state == ListParseState::Index && value == 0x00 {
            self.list_state = ListParseState::Value;
            self.utf8_string.clear();
        } else if self.list_state == ListParseState::Value && value != 0x00 {
            self.utf8_string.push(value);
        } else if self.list_state == ListParseState::Value {
            let item = String::from_utf8_lossy(&self.utf8_string).into_owned();
            self.list_value.push(item);
            self.list_state = ListParseState::Waiting;
            self.utf8_string.clear();
        }
    }

    /// `ENDING::handleState`: 3 Bytes nach Boolean/Date-Wert verbrauchen.
    fn handle_ending(&mut self, value: u8) {
        if (self.field_type == FieldType::Boolean || self.field_type == FieldType::Date)
            && self.ending_buffer.len() < 2
        {
            self.ending_buffer.push(value);
        } else if self.ending_buffer.len() == 2 {
            self.ending_buffer.clear();
            self.state = ParseState::Waiting;
        }
    }
}

/// State Machine über die Bytes NACH dem 11-Byte-Headers laufen lassen.
fn run_shortcut_state_machine(buffer: &[u8]) -> Vec<SteamShortcutEntry> {
    let mut sm = ShortcutVdfStateMachine::new();
    for &b in buffer {
        sm.handle_byte(b);
    }
    sm.shortcuts
}

/// Komplette shortcuts.vdf-Datei parsen (Header-Check wie `parseShortcuts`).
pub fn parse_shortcuts_vdf(data: &[u8]) -> Result<Vec<SteamShortcutEntry>, SteamError> {
    if data.len() < 16 {
        info!("shortcut file not valid");
        return Ok(Vec::new());
    }
    if data[..SHORTCUT_HEADER.len()] != SHORTCUT_HEADER {
        info!("shortcut file not valid, incorrect header");
        return Ok(Vec::new());
    }
    Ok(run_shortcut_state_machine(&data[SHORTCUT_HEADER.len()..]))
}

/// Ein Shortcut-Attribut binär schreiben (`writeShortcutAttribute`).
/// Typ-Byte: LIST=0x00, STRING=0x01, alles andere (BOOLEAN/DATE/APPID)=0x02.
fn write_shortcut_attribute(out: &mut Vec<u8>, field_type: FieldType, key: &str, value: &str) {
    match field_type {
        FieldType::List => out.push(0x00),
        FieldType::String => out.push(0x01),
        _ => out.push(0x02),
    }
    out.extend_from_slice(key.as_bytes());
    out.push(0x00);
    match field_type {
        FieldType::String => {
            out.extend_from_slice(value.as_bytes());
            out.push(0x00); // Endesequenz String
        }
        FieldType::Boolean => {
            // 0x01 für true, 0x00 für false; 3 Auffüll-Bytes
            out.push(if value == "true" { 0x01 } else { 0x00 });
            out.extend_from_slice(&[0x00, 0x00, 0x00]);
        }
        FieldType::Date => {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        }
        FieldType::AppId => {
            let id: u32 = value.parse().unwrap_or(0); // wie QString::toUInt (invalid → 0)
            out.extend_from_slice(&id.to_le_bytes());
        }
        FieldType::List => {
            for (index, element) in value.split(',').enumerate() {
                out.push(0x01); // List-Entry-Marker
                out.extend_from_slice(index.to_string().as_bytes()); // Index als Dezimalstring
                out.push(0x00); // Trenner
                out.extend_from_slice(element.as_bytes());
                out.push(0x00); // Entry-Ende
            }
            out.push(0x08);
            out.push(0x08);
        }
    }
}

/// Komplette shortcuts.vdf-Datei schreiben (`updateShortcuts`-Schreibteil).
pub fn write_shortcuts_vdf(shortcuts: &[SteamShortcutEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x00);
    out.extend_from_slice(b"shortcuts");
    out.push(0x00);
    for (entry_id, shortcut) in shortcuts.iter().enumerate() {
        out.push(0x00);
        out.extend_from_slice(entry_id.to_string().as_bytes());
        out.push(0x00);
        // QMap-Iteration: sortiert nach (Lowercase-)Key — BTreeMap liefert das 1:1
        for (key, property) in &shortcut.properties {
            let real_key = shortcut.keys.get(key).map(String::as_str).unwrap_or(key);
            write_shortcut_attribute(&mut out, property.field_type, real_key, &property.value);
        }
    }
    out.push(0x08);
    out.push(0x08);
    out
}

// ---------------------------------------------------------------------------
// Steam-Locations (Registry, loginusers.vdf, libraryfolders.vdf)
// ---------------------------------------------------------------------------

/// Steam-Installationspfad aus der Registry lesen
/// (`HKCU\Software\Valve\Steam` → `SteamPath`).
#[cfg(windows)]
pub fn steam_path_from_registry() -> Result<PathBuf, SteamError> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key_path = PathBuf::from(r"HKCU\Software\Valve\Steam");
    let key = hkcu
        .open_subkey("Software\\Valve\\Steam")
        .map_err(|e| SteamError::Io {
            path: key_path.clone(),
            source: e,
        })?;
    let steam_path: String = key.get_value("SteamPath").map_err(|e| SteamError::Io {
        path: key_path.join("SteamPath"),
        source: e,
    })?;
    Ok(PathBuf::from(steam_path))
}

/// Registry-Lookup nur unter Windows verfügbar.
#[cfg(not(windows))]
pub fn steam_path_from_registry() -> Result<PathBuf, SteamError> {
    Err(SteamError::InvalidArgument(
        "Steam registry lookup is only available on Windows".into(),
    ))
}

/// Steam-Basisverzeichnis ermitteln: expliziter Pfad (C++-Konstruktor-Parameter
/// `steamDir`) > Registry `SteamPath` > C++-Fallback `C:/Program Files (x86)/Steam`.
pub fn find_steam_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(dir) = explicit {
        if !dir.as_os_str().is_empty() {
            return dir.to_path_buf();
        }
    }
    #[cfg(windows)]
    match steam_path_from_registry() {
        Ok(path) => return path,
        Err(e) => debug!("Steam registry lookup failed: {e}"),
    }
    PathBuf::from("C:/Program Files (x86)/Steam")
}

/// `atoll`-Semantik: führende Dezimalziffern parsen, sonst 0.
fn atoll(s: &str) -> u64 {
    let t = s.trim_start();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    t[..digits].parse().unwrap_or(0)
}

/// `getMostRecentUser`: loginusers.vdf lesen und die SteamID des zuletzt
/// aktiven Users finden; daraus die userdata-Account-ID machen
/// (steamid64 − 76561197960265728).
pub fn most_recent_user(steam_dir: &Path) -> Result<String, SteamError> {
    let path = steam_dir.join("config").join("loginusers.vdf");
    let content = match fs::read(&path) {
        Ok(content) => content,
        // C++: Öffnen fehlgeschlagen → Fehler, kein User
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SteamError::NoSteamUser { path })
        }
        Err(e) => return Err(SteamError::Io { path, source: e }),
    };
    let content = String::from_utf8_lossy(&content);

    let mut steamid: u64 = 0;
    let mut user_id = String::new();
    for line in content.lines() {
        if line.contains("7656119") && !line.contains("PersonalName") {
            // C++: mid(indexOf("7656119"), …) — bis Zeilenende; atoll stoppt am '"'
            if let Some(pos) = line.find("7656119") {
                steamid = atoll(&line[pos..]);
            }
        } else if line.to_lowercase().contains("mostrecent") && line.contains("\"1\"") {
            // C++: atoll("")==0 → wrapping_sub reproduziert den unsigned overflow
            user_id = steamid.wrapping_sub(76561197960265728).to_string();
        }
    }

    if user_id.is_empty() {
        return Err(SteamError::NoSteamUser { path });
    }
    Ok(user_id)
}

/// Zeitstempel für Backup-Dateien. C++ nutzt `yyyy-MM-dd-HH:mm:ss` — der Doppelpunkt
/// ist in Windows-Dateinamen aber unzulässig, `QFile::copy` schlägt dort still fehl
/// (C++-Bug). Wir ersetzen ihn durch '-' (`yyyy-MM-dd-HH-mm-ss`), UTC statt Lokalzeit.
fn backup_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}-{:02}-{:02}-{:02}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Tage seit Epoch → (Jahr, Monat, Tag); Howard Hinnants `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Shortcut-Verwaltung (SteamTools-API + qmlbackend-Parameterlogik)
// ---------------------------------------------------------------------------

/// Workshop-ID des chiaki-Controller-Layouts (qmlbackend.cpp:
/// `controller_layout_workshop_id = "3049833406"`, Steam-Deck/Neptune-Config).
pub const CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID: &str = "3049833406";

/// Tags, die qmlbackend.cpp jedem Shortcut gibt.
pub const DEFAULT_STEAM_SHORTCUT_TAGS: &[&str] = &["PlayStation", "Remote Play"];

/// Wunsch-Shortcut für [`SteamShortcuts::add_shortcut`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppShortcut {
    /// Anzeigename in Steam (chiaki-ng-Dialog-Default: "Chiaki Remaster").
    pub app_name: String,
    /// Pfad zur Exe (wird quoted in die Datei geschrieben).
    pub exe: PathBuf,
    /// Arbeitsverzeichnis; `None` → Verzeichnis der Exe
    /// (C++: `QFileInfo(filepath).absolutePath()`).
    pub start_dir: Option<PathBuf>,
    /// Optionale Icon-Datei; wird ins Steam-Grid-Verzeichnis kopiert.
    pub icon: Option<PathBuf>,
    /// Launch-Options (z. B. `--profile=MeinProfil`).
    pub launch_options: String,
    /// Steam-Tags; leer → [`DEFAULT_STEAM_SHORTCUT_TAGS`].
    pub tags: Vec<String>,
}

/// Art eines Grid-Images (C++-artworkLocations in buildShortcutEntry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkKind {
    Icon,
    Landscape,
    Portrait,
    Hero,
    Logo,
}

impl ArtworkKind {
    fn name(self) -> &'static str {
        match self {
            ArtworkKind::Icon => "icon",
            ArtworkKind::Landscape => "landscape",
            ArtworkKind::Portrait => "portrait",
            ArtworkKind::Hero => "hero",
            ArtworkKind::Logo => "logo",
        }
    }
}

/// Grid-Image-Dateiname für eine Short-App-ID
/// (`"%1/userdata/%2/config/grid/%3…"`, C++ artworkLocations).
pub fn grid_file_name(short_app_id: &str, kind: ArtworkKind) -> String {
    match kind {
        ArtworkKind::Icon => format!("{short_app_id}_icon.png"),
        ArtworkKind::Landscape => format!("{short_app_id}.png"),
        ArtworkKind::Portrait => format!("{short_app_id}p.png"),
        ArtworkKind::Hero => format!("{short_app_id}_hero.png"),
        ArtworkKind::Logo => format!("{short_app_id}_logo.png"),
    }
}

/// Grid-Artwork als kodiertes PNG (C++ übergibt `QPixmap*` und speichert PNGs;
/// hier liefert der Aufrufer die PNG-Bytes, z. B. via `include_bytes!`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Artwork<'a> {
    /// `<id>_icon.png` — wird zusätzlich als Icon-Eigenschaft des Shortcuts gesetzt.
    pub icon: Option<&'a [u8]>,
    /// `<id>.png` (Breitbild-Kachel)
    pub landscape: Option<&'a [u8]>,
    /// `<id>p.png` (Hochkant-Kachel)
    pub portrait: Option<&'a [u8]>,
    /// `<id>_hero.png` (Header)
    pub hero: Option<&'a [u8]>,
    /// `<id>_logo.png` (Logo)
    pub logo: Option<&'a [u8]>,
}

impl Artwork<'_> {
    /// Alle gesetzten Bilder als (Art, Bytes).
    pub fn iter(&self) -> impl Iterator<Item = (ArtworkKind, &[u8])> {
        [
            (ArtworkKind::Icon, self.icon),
            (ArtworkKind::Landscape, self.landscape),
            (ArtworkKind::Portrait, self.portrait),
            (ArtworkKind::Hero, self.hero),
            (ArtworkKind::Logo, self.logo),
        ]
        .into_iter()
        .filter_map(|(kind, data)| data.map(|d| (kind, d)))
    }
}

/// Ergebnis eines Adds: neu angelegt oder bestehender Eintrag ersetzt
/// (Match-Kriterium wie qmlbackend.cpp: gleiche Exe + gleiche Launch-Options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteamShortcutAction {
    Added,
    Updated,
}

/// Zugriff auf die Steam-Shortcut-Verwaltung eines Users
/// (`SteamTools` + `QmlBackend::createSteamShortcut`).
#[derive(Debug, Clone)]
pub struct SteamShortcuts {
    steam_dir: PathBuf,
    user_id: String,
    shortcut_file: PathBuf,
}

impl SteamShortcuts {
    /// Steam finden (expliziter Pfad > Registry > Fallback) und den zuletzt
    /// aktiven User bestimmen. Fehler, wenn das Steam-Verzeichnis nicht existiert
    /// oder kein User ermittelbar ist (qmlbackend prüft beides vor dem Add).
    pub fn open(steam_dir: Option<&Path>) -> Result<Self, SteamError> {
        Self::at(find_steam_dir(steam_dir))
    }

    /// Wie [`SteamShortcuts::open`], aber mit fest vorgegebenem Basisverzeichnis
    /// (entspricht `SteamTools(…, steamDir)`; für Tests/Portable-Steam).
    pub fn at(steam_dir: impl Into<PathBuf>) -> Result<Self, SteamError> {
        let steam_dir = steam_dir.into();
        if !steam_dir.is_dir() {
            return Err(SteamError::SteamNotFound { path: steam_dir });
        }
        let user_id = most_recent_user(&steam_dir)?;
        let shortcut_file = steam_dir
            .join("userdata")
            .join(&user_id)
            .join("config")
            .join("shortcuts.vdf");
        Ok(Self {
            steam_dir,
            user_id,
            shortcut_file,
        })
    }

    pub fn steam_dir(&self) -> &Path {
        &self.steam_dir
    }

    /// userdata-Account-ID des zuletzt aktiven Steam-Users.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Pfad zu `userdata/<uid>/config/shortcuts.vdf`.
    pub fn shortcut_file(&self) -> &Path {
        &self.shortcut_file
    }

    /// `steamExists()`: existiert das Steam-Basisverzeichnis?
    pub fn steam_exists(&self) -> bool {
        self.steam_dir.is_dir()
    }

    /// `parseShortcuts()`: alle Non-Steam-Shortcuts des Users lesen.
    ///
    /// Fehlende Datei → leere Liste (mit Warnung) — so legt der erste Add sauber an.
    /// Ungültiger Header/zu kurze Datei → leere Liste (C++: info-Callback).
    pub fn all_shortcuts(&self) -> Result<Vec<SteamShortcutEntry>, SteamError> {
        let data = match fs::read(&self.shortcut_file) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!("Error opening file: {}", self.shortcut_file.display());
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(SteamError::Io {
                    path: self.shortcut_file.clone(),
                    source: e,
                })
            }
        };
        parse_shortcuts_vdf(&data)
    }

    /// Existiert bereits ein Shortcut mit diesem Namen oder dieser Exe?
    pub fn is_installed(&self, app_name: &str, exe: &str) -> Result<bool, SteamError> {
        Ok(self.all_shortcuts()?.iter().any(|s| {
            s.app_name() == Some(app_name)
                || s.exe_unquoted() == Some(exe)
                || s.exe() == Some(exe)
        }))
    }

    /// `updateShortcuts()`: shortcuts.vdf mit der übergebenen Liste überschreiben
    /// (vorher Backup `shortcuts.vdf.<yyyy-MM-dd-HH:mm:ss>.bak`).
    pub fn update_shortcuts(&self, shortcuts: &[SteamShortcutEntry]) -> Result<(), SteamError> {
        if self.shortcut_file.exists() {
            let backup = self
                .shortcut_file
                .with_file_name(format!("shortcuts.vdf.{}.bak", backup_timestamp()));
            match fs::copy(&self.shortcut_file, &backup) {
                Ok(_) => info!("shortcuts.vdf backed up at '{}'", backup.display()),
                Err(e) => error!("Error backing up shortcuts.vdf: {e}"),
            }
        }
        let data = write_shortcuts_vdf(shortcuts);
        fs::write(&self.shortcut_file, data).map_err(|e| SteamError::Io {
            path: self.shortcut_file.clone(),
            source: e,
        })?;
        info!("File '{}' updated successfully.", self.shortcut_file.display());
        Ok(())
    }

    /// Shortcut hinzufügen oder (bei gleicher Exe + Launch-Options) ersetzen
    /// (Port von `buildShortcutEntry` + `QmlBackend::createSteamShortcut`-Match-Logik).
    ///
    /// `artwork` (Grid-Images) entspricht der QPixmap-Map im C++; das Icon-Bild
    /// landet im Grid-Verzeichnis und wird als Icon-Eigenschaft eingetragen.
    /// Ohne Artwork wird stattdessen `AppShortcut::icon` kopiert, falls gesetzt.
    pub fn add_shortcut(
        &self,
        shortcut: &AppShortcut,
        artwork: Option<&Artwork>,
    ) -> Result<SteamShortcutAction, SteamError> {
        let new_entry = self.build_shortcut_entry(shortcut, artwork)?;
        let exe_quoted = quoted_exe(&shortcut.exe);

        let mut shortcuts = self.all_shortcuts()?;
        let mut action = SteamShortcutAction::Added;
        let mut found = false;
        for existing in shortcuts.iter_mut() {
            // qmlbackend.cpp: gleiche Exe UND gleiche Launch-Options → ersetzen
            if existing.exe().unwrap_or("") == exe_quoted
                && existing.launch_options().unwrap_or("") == shortcut.launch_options
            {
                info!("Updating Steam entry");
                *existing = new_entry.clone();
                found = true;
                action = SteamShortcutAction::Updated;
                break;
            }
        }
        if !found {
            info!("Adding Steam entry {}", shortcut.app_name);
            shortcuts.push(new_entry);
        }
        self.update_shortcuts(&shortcuts)?;

        // qmlbackend.cpp setzt zusätzlich das offizielle Controller-Layout;
        // ein Fehler hier macht den Shortcut-Vorgang nicht kaputt.
        if let Err(e) =
            self.update_controller_config(&shortcut.app_name, CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID)
        {
            warn!("Could not update Steam controller config: {e}");
        }
        Ok(action)
    }

    /// "Add to Steam library" — 1:1-Port von `QmlBackend::createSteamShortcut`:
    /// Exe = laufendes Programm (`QCoreApplication::applicationFilePath()`),
    /// StartDir = Exe-Verzeichnis, Tags = PlayStation/Remote Play, Grid-Images
    /// aus `artwork` (im C++: eingebettete Qt-Ressourcen `:/icons/steam_*.png`),
    /// danach Controller-Layout `3049833406` setzen.
    pub fn add_to_library(
        &self,
        app_name: &str,
        launch_options: &str,
        artwork: &Artwork,
    ) -> Result<SteamShortcutAction, SteamError> {
        if !self.steam_exists() {
            return Err(SteamError::SteamNotFound {
                path: self.steam_dir.clone(),
            });
        }
        let exe = std::env::current_exe().map_err(|e| SteamError::IoOther { source: e })?;
        let shortcut = AppShortcut {
            app_name: app_name.to_string(),
            exe,
            start_dir: None,  // → Exe-Verzeichnis (QFileInfo::absolutePath)
            icon: None,       // Icon kommt aus dem Grid-Artwork ("icon")
            launch_options: launch_options.to_string(),
            tags: DEFAULT_STEAM_SHORTCUT_TAGS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        self.add_shortcut(&shortcut, Some(artwork))
    }

    /// Shortcuts entfernen; Kriterium: Name und/oder (unquoted) Exe.
    /// Ein Eintrag zählt als Treffer, wenn er auf EINES der angegebenen
    /// Kriterien passt. `Ok(false)`, wenn nichts entfernt wurde.
    pub fn remove_shortcut(
        &self,
        app_name: Option<&str>,
        exe: Option<&str>,
    ) -> Result<bool, SteamError> {
        if app_name.is_none() && exe.is_none() {
            return Err(SteamError::InvalidArgument(
                "remove_shortcut needs an app_name and/or exe".into(),
            ));
        }
        let shortcuts = self.all_shortcuts()?;
        let mut changed = false;
        let kept: Vec<SteamShortcutEntry> = shortcuts
            .into_iter()
            .filter(|s| {
                let matches = match (app_name, exe) {
                    (Some(name), Some(path)) => {
                        s.app_name() == Some(name)
                            || s.exe_unquoted() == Some(path)
                            || s.exe() == Some(path)
                    }
                    (Some(name), None) => s.app_name() == Some(name),
                    (None, Some(path)) => {
                        s.exe_unquoted() == Some(path) || s.exe() == Some(path)
                    }
                    (None, None) => false,
                };
                if matches {
                    changed = true;
                    false // entfernen
                } else {
                    true
                }
            })
            .collect();
        if changed {
            self.update_shortcuts(&kept)?;
        }
        Ok(changed)
    }

    /// `updateControllerConfig`: dem Shortcut das offizielle Steam-Deck-
    /// Controller-Layout zuweisen (`configset_controller_neptune.vdf`).
    /// Fehlt die Datei → Warnung und Ok (C++ bricht nur den Config-Teil ab).
    pub fn update_controller_config(
        &self,
        app_name: &str,
        controller_config_id: &str,
    ) -> Result<(), SteamError> {
        let controller_file = self
            .steam_dir
            .join("steamapps")
            .join("common")
            .join("Steam Controller Configs")
            .join(&self.user_id)
            .join("config")
            .join("configset_controller_neptune.vdf");
        if !controller_file.exists() {
            warn!("Neptune controller config not found, not adding");
            return Ok(());
        }

        let raw = fs::read(&controller_file).map_err(|e| SteamError::Io {
            path: controller_file.clone(),
            source: e,
        })?;
        let content = String::from_utf8_lossy(&raw).into_owned();
        let mut root = parse_vdf(content.as_bytes())?;

        // C++: appName.toLower()
        let app_name_lower = app_name.to_lowercase();
        let mut update_file = false;

        if let Some(old_entry) = root.childs.get_mut(&app_name_lower) {
            // Workshop-Eintrag ergänzen, falls fehlend
            if !old_entry.attribs.contains_key("workshop") {
                old_entry
                    .attribs
                    .insert("workshop".into(), controller_config_id.to_string());
                update_file = true;
            }
            // Template-Einstellung löschen, damit die Workshop-ID greift
            if old_entry.attribs.remove("template").is_some() {
                update_file = true;
            } else if old_entry.attribs.get("workshop").map(String::as_str)
                == Some(controller_config_id)
            {
                info!(
                    "Controller config already set for {}, not overwriting",
                    app_name_lower
                );
            } else {
                old_entry
                    .attribs
                    .insert("workshop".into(), controller_config_id.to_string());
                update_file = true;
            }
        } else {
            info!(
                "Setting {} to use the official controller config with workshop ID {} for the Steam Deck controller",
                app_name_lower, controller_config_id
            );
            let mut entry = VdfObject {
                name: app_name_lower.clone(),
                attribs: BTreeMap::new(),
                childs: BTreeMap::new(),
            };
            entry.add_attribute("workshop", controller_config_id);
            root.add_child(entry);
            update_file = true;
        }

        if update_file {
            // Backup wie im C++ (configset_controller_neptune.<date>.bak)
            let backup = controller_file
                .with_file_name(format!("configset_controller_neptune.{}.bak", backup_timestamp()));
            match fs::copy(&controller_file, &backup) {
                Ok(_) => info!(
                    "{} backed up at {}",
                    controller_file.display(),
                    backup.display()
                ),
                Err(e) => error!("Error backing up {}: {e}", controller_file.display()),
            }
            let out = write_vdf(&root);
            fs::write(&controller_file, out).map_err(|e| SteamError::Io {
                path: controller_file.clone(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// `libraryfolders.vdf` lesen → alle Library-Pfade (UTF-8-sicher; die im
    /// File doppelt escapten Backslashes werden vom VDF-Parser aufgelöst).
    pub fn library_folders(&self) -> Result<Vec<PathBuf>, SteamError> {
        let path = self.steam_dir.join("steamapps").join("libraryfolders.vdf");
        if !path.exists() {
            debug!("libraryfolders.vdf not found at {}", path.display());
            return Ok(Vec::new());
        }
        let raw = fs::read(&path).map_err(|e| SteamError::Io {
            path: path.clone(),
            source: e,
        })?;
        let content = String::from_utf8_lossy(&raw);
        let root = parse_vdf(content.as_bytes())?;
        Ok(root
            .childs
            .values()
            .filter_map(|child| child.attribs.get("path"))
            .map(PathBuf::from)
            .collect())
    }

    /// `buildShortcutEntry`: Shortcut-Entry mit den qmlbackend-Parametern bauen
    /// (inkl. Grid-Images und CRC-basierter Shortcut-ID).
    fn build_shortcut_entry(
        &self,
        shortcut: &AppShortcut,
        artwork: Option<&Artwork>,
    ) -> Result<SteamShortcutEntry, SteamError> {
        let exe_path = shortcut.exe.to_string_lossy().into_owned();
        let exe_quoted = format!("\"{exe_path}\"");
        let short_app_id = generate_short_app_id(&exe_quoted, &shortcut.app_name);

        // Grid-Verzeichnis anlegen (C++: mkpath "%1/userdata/%2/config/grid")
        let grid_dir = self
            .steam_dir
            .join("userdata")
            .join(&self.user_id)
            .join("config")
            .join("grid");
        fs::create_dir_all(&grid_dir).map_err(|e| SteamError::Io {
            path: grid_dir.clone(),
            source: e,
        })?;

        // Artwork speichern (Grid-Images wie in saveArtwork)
        let mut icon_path: Option<PathBuf> = None;
        if let Some(art) = artwork {
            for (kind, data) in art.iter() {
                let file = grid_dir.join(grid_file_name(&short_app_id, kind));
                fs::write(&file, data).map_err(|e| SteamError::Io {
                    path: file.clone(),
                    source: e,
                })?;
                info!("Saved {} to {}", kind.name(), file.display());
                if kind == ArtworkKind::Icon {
                    icon_path = Some(file);
                }
            }
        }
        if icon_path.is_none() {
            // Kein eingebettetes Icon → ggf. Icon-Datei aus AppShortcut kopieren
            if let Some(icon_src) = &shortcut.icon {
                let file = grid_dir.join(grid_file_name(&short_app_id, ArtworkKind::Icon));
                fs::copy(icon_src, &file).map_err(|e| SteamError::Io {
                    path: file.clone(),
                    source: e,
                })?;
                icon_path = Some(file);
            }
        }

        let mut entry = SteamShortcutEntry::default();
        if let Some(icon) = icon_path {
            entry.set_property(
                "icon",
                "icon",
                ShortcutProperty::new(FieldType::String, icon.to_string_lossy().into_owned()),
            );
        }
        entry.set_property(
            "appid",
            "appid",
            ShortcutProperty::new(
                FieldType::AppId,
                generate_shortcut_id(&exe_quoted, &shortcut.app_name).to_string(),
            ),
        );
        entry.set_property(
            "appname",
            "AppName",
            ShortcutProperty::new(FieldType::String, shortcut.app_name.clone()),
        );
        entry.set_property(
            "exe",
            "Exe",
            ShortcutProperty::new(FieldType::String, exe_quoted),
        );
        let start_dir = shortcut
            .start_dir
            .clone()
            .unwrap_or_else(|| shortcut.exe.parent().map(Path::to_path_buf).unwrap_or_default());
        entry.set_property(
            "startdir",
            "StartDir",
            ShortcutProperty::new(FieldType::String, start_dir.to_string_lossy().into_owned()),
        );
        entry.set_property(
            "shortcutpath",
            "ShortcutPath",
            ShortcutProperty::new(FieldType::String, ""),
        );
        entry.set_property(
            "launchoptions",
            "LaunchOptions",
            ShortcutProperty::new(FieldType::String, shortcut.launch_options.clone()),
        );
        for (key, real_key) in [
            ("ishidden", "IsHidden"),
            ("allowdesktopconfig", "AllowDesktopConfig"),
            ("allowoverlay", "AllowOverlay"),
            ("openvr", "OpenVR"),
            ("devkit", "DevKit"),
            ("devkitoverrideappid", "DevkitOverrideAppID"),
        ] {
            entry.set_property(key, real_key, ShortcutProperty::new(FieldType::Boolean, ""));
        }
        entry.set_property(
            "devkitgameid",
            "DevkitGameID",
            ShortcutProperty::new(FieldType::String, ""),
        );
        entry.set_property(
            "lastplaytime",
            "LastPlayTime",
            ShortcutProperty::new(FieldType::Date, ""),
        );
        entry.set_property(
            "flatpakappid",
            "FlatpakAppID",
            ShortcutProperty::new(FieldType::String, ""),
        );
        let tags: Vec<String> = if shortcut.tags.is_empty() {
            DEFAULT_STEAM_SHORTCUT_TAGS.iter().map(|s| s.to_string()).collect()
        } else {
            shortcut.tags.clone()
        };
        entry.set_property(
            "tags",
            "tags",
            ShortcutProperty::new(FieldType::List, tags.join(",")),
        );
        Ok(entry)
    }
}

/// Exe-Pfad in Anführungszeichen (C++: `"\"" + filepath + "\""`).
fn quoted_exe(exe: &Path) -> String {
    format!("\"{}\"", exe.to_string_lossy())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Temp-Verzeichnis mit Aufräumen (keine echten Steam-Schreibzugriffe).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "chiaki-app-steam-{}-{}-{}",
                tag,
                std::process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Fake-Steam-Verzeichnis mit loginusers.vdf (User 76561198000000000 ist "most recent").
    fn fake_steam(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        let config = dir.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("loginusers.vdf"),
            "\"users\"\n{\n\t\"76561198000000000\"\n\t{\n\t\t\"name\"\t\t\"alice\"\n\t\t\
             \"PersonalName\"\t\t\"alice\"\n\t\t\"MostRecent\"\t\t\"1\"\n\t\t\
             \"Timestamp\"\t\t\"1234\"\n\t}\n\t\"76561198011111111\"\n\t{\n\t\t\
             \"PersonalName\"\t\t\"bob\"\n\t}\n}\n",
        )
        .unwrap();
        dir
    }

    fn test_shortcut(exe: &str, launch_options: &str) -> AppShortcut {
        AppShortcut {
            app_name: "Chiaki Remaster".into(),
            exe: PathBuf::from(exe),
            start_dir: None,
            icon: None,
            launch_options: launch_options.into(),
            tags: DEFAULT_STEAM_SHORTCUT_TAGS.iter().map(|s| s.to_string()).collect(),
        }
    }

    // --- CRC32 / IDs ---

    #[test]
    fn test_crc32_golden() {
        // Standard-Check-Wert von CRC-32/ISO-HDLC (zlib.crc32)
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        assert_eq!(crc32(b""), 0x00000000);
        // per zlib.crc32 nachgerechnete Vektoren
        assert_eq!(crc32(b"\"C:\\Games\\chiaki.exe\"Chiaki Remaster"), 0x19623236);
        assert_eq!(
            crc32(b"\"C:\\Program Files\\chiaki\\chiaki.exe\"Chiaki Remaster"),
            0xa6776126
        );
    }

    #[test]
    fn test_shortcut_id_golden() {
        // exe (quoted!) + appname, wie im C++ generiert
        let exe = "\"C:\\Games\\chiaki.exe\"";
        assert_eq!(crc32(format!("{exe}Chiaki Remaster").as_bytes()), 0x19623236);
        // preliminary = (crc | 0x80000000) << 32 | 0x02000000
        assert_eq!(generate_preliminary_id(exe, "Chiaki Remaster"), 0x99623236_02000000);
        assert_eq!(generate_app_id(exe, "Chiaki Remaster"), "11052451643063795712");
        // short app id == (prelim >> 32); shortcut id == selbiges als u32 (Wrap von -2^32)
        assert_eq!(generate_short_app_id(exe, "Chiaki Remaster"), "2573349430");
        assert_eq!(generate_shortcut_id(exe, "Chiaki Remaster"), 0x99623236);
        assert_eq!(generate_shortcut_id(exe, "Chiaki Remaster"), 2573349430);

        let exe2 = "\"C:\\Program Files\\chiaki\\chiaki.exe\"";
        assert_eq!(
            generate_preliminary_id(exe2, "Chiaki Remaster"),
            0xa6776126_02000000
        );
        assert_eq!(generate_shortcut_id(exe2, "Chiaki Remaster"), 0xa6776126);
    }

    #[test]
    fn test_atoll_semantics() {
        assert_eq!(atoll("76561197960265728\""), 76561197960265728);
        assert_eq!(atoll("  42abc"), 42);
        assert_eq!(atoll(""), 0);
        // C++: 0u64.wrapping_sub(76561197960265728) — unsigned overflow wie im Original
        assert_eq!(
            0u64.wrapping_sub(76561197960265728),
            u64::MAX - 76561197960265728 + 1
        );
    }

    // --- Text-VDF ---

    #[test]
    fn test_text_vdf_parse_golden() {
        let src = r#"// leading comment
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\Steam"
		/* block
		   comment */
		"apps"
		{
			"12345"		"1240000000"
		}
	}
	"1"		{ "path"	"D:\\SteamLibrary" } // trailing comment
	"linux-only"	"skipped"	[$LINUX]
	"win-only"	"kept"	[$WIN32]
	"not-on-win"	"skipped"	[!$WIN32]
	"esc"	"quote:\" back:\\ end"
	unquotedkey	unquotedvalue
}
"#;
        let root = parse_vdf(src.as_bytes()).unwrap();
        assert_eq!(root.name, "libraryfolders");
        assert_eq!(root.childs.len(), 2);
        assert_eq!(
            root.childs["0"].attribs["path"],
            "C:\\Program Files (x86)\\Steam"
        );
        assert_eq!(root.childs["0"].childs["apps"].attribs["12345"], "1240000000");
        assert_eq!(root.childs["1"].attribs["path"], "D:\\SteamLibrary");
        // Konditionale: $WIN32 erfüllt, ! $WIN32 und $LINUX nicht
        assert_eq!(root.attribs.get("win-only").map(String::as_str), Some("kept"));
        assert!(root.attribs.get("linux-only").is_none());
        assert!(root.attribs.get("not-on-win").is_none());
        // Escapes: nur \" und \\ werden ersetzt
        assert_eq!(root.attribs["esc"], "quote:\" back:\\ end");
        // unquoted Tokens
        assert_eq!(root.attribs["unquotedkey"], "unquotedvalue");
    }

    #[test]
    fn test_text_vdf_write_golden_and_roundtrip() {
        let mut child = VdfObject::default();
        child.name = "child".into();
        child.add_attribute("a", "b");
        let mut root = VdfObject::default();
        root.name = "root".into();
        root.add_attribute("key", "value");
        root.add_child(child);

        // 1:1-Format von tyti::vdf::write
        assert_eq!(
            write_vdf(&root),
            "\"root\"\n{\n\t\"key\"\t\t\"value\"\n\t\"child\"\n\t{\n\t\t\"a\"\t\t\"b\"\n\t}\n}\n"
        );

        let round = parse_vdf(write_vdf(&root).as_bytes()).unwrap();
        assert_eq!(round, root);
    }

    #[test]
    fn test_text_vdf_parse_errors() {
        // unquoted Wort am Dateiende ohne Whitespace → "word wasnt properly ended" (wie C++)
        assert!(parse_vdf(b"key value").is_err());
        // nicht geschlossenes Quote
        assert!(parse_vdf(b"\"k\" \"v\"").is_err());
        // Objekt nicht geschlossen
        assert!(parse_vdf(b"\"root\"\n{\n\"a\" \"b\"\n").is_err());
        // '}' ohne Objekt
        assert!(parse_vdf(b"}").is_err());
    }

    // --- Binäres shortcuts.vdf ---

    #[test]
    fn test_binary_shortcuts_vdf_golden_bytes() {
        let mut entry = SteamShortcutEntry::default();
        entry.set_property("appid", "appid", ShortcutProperty::new(FieldType::AppId, "2573349430"));
        entry.set_property("appname", "AppName", ShortcutProperty::new(FieldType::String, "Chiaki Remaster"));
        entry.set_property("exe", "Exe", ShortcutProperty::new(FieldType::String, "\"C:\\Games\\chiaki.exe\""));
        entry.set_property("ishidden", "IsHidden", ShortcutProperty::new(FieldType::Boolean, "false"));
        entry.set_property("tags", "tags", ShortcutProperty::new(FieldType::List, "PlayStation,Remote Play"));

        let out = write_shortcuts_vdf(&[entry.clone()]);

        // Golden-Bytes, unabhängig vom Writer zusammengebaut
        let mut want: Vec<u8> = Vec::new();
        want.extend_from_slice(&SHORTCUT_HEADER);
        want.push(0x00); // Entry 0
        want.extend_from_slice(b"0");
        want.push(0x00);
        // appid: Typ 0x02, u32 little-endian
        want.extend_from_slice(&[0x02]);
        want.extend_from_slice(b"appid");
        want.push(0x00);
        want.extend_from_slice(&2573349430u32.to_le_bytes());
        // appname: Typ 0x01 — als Key wird der REAL-Key geschrieben (keys.value → "AppName")
        want.extend_from_slice(&[0x01]);
        want.extend_from_slice(b"AppName");
        want.push(0x00);
        want.extend_from_slice(b"Chiaki Remaster");
        want.push(0x00);
        // exe
        want.extend_from_slice(&[0x01]);
        want.extend_from_slice(b"Exe");
        want.push(0x00);
        want.extend_from_slice(b"\"C:\\Games\\chiaki.exe\"");
        want.push(0x00);
        // ishidden: Typ 0x02 + Wert-Byte 0x00 + 3 Nullbytes
        want.extend_from_slice(&[0x02]);
        want.extend_from_slice(b"IsHidden");
        want.push(0x00);
        want.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        // tags: LIST, Indizes als Dezimalstrings
        want.extend_from_slice(&[0x00]);
        want.extend_from_slice(b"tags");
        want.push(0x00);
        want.extend_from_slice(&[0x01]);
        want.extend_from_slice(b"0");
        want.push(0x00);
        want.extend_from_slice(b"PlayStation");
        want.push(0x00);
        want.extend_from_slice(&[0x01]);
        want.extend_from_slice(b"1");
        want.push(0x00);
        want.extend_from_slice(b"Remote Play");
        want.push(0x00);
        want.extend_from_slice(&[0x08, 0x08]);
        // Dateiende
        want.extend_from_slice(&[0x08, 0x08]);

        assert_eq!(out, want);

        // Roundtrip über den Parser (Header-Check inklusive)
        let parsed = parse_shortcuts_vdf(&out).unwrap();
        assert_eq!(parsed, vec![entry]);
    }

    #[test]
    fn test_binary_shortcuts_vdf_rejects_invalid() {
        // zu kurz / falscher Header → leere Liste (C++-Verhalten)
        assert!(parse_shortcuts_vdf(&[0u8; 8]).unwrap().is_empty());
        let mut bad = SHORTCUT_HEADER.to_vec();
        bad[1] = b'X';
        bad.extend_from_slice(&[0u8; 32]);
        assert!(parse_shortcuts_vdf(&bad).unwrap().is_empty());
    }

    // --- Steam-Locations ---

    #[test]
    fn test_most_recent_user() {
        let dir = fake_steam("loginusers");
        let uid = most_recent_user(dir.path()).unwrap();
        // 76561198000000000 - 76561197960265728 = 39734272
        assert_eq!(uid, "39734272");
    }

    #[test]
    fn test_most_recent_user_missing_file_is_error() {
        let dir = TempDir::new("no-loginusers");
        match most_recent_user(dir.path()) {
            Err(SteamError::NoSteamUser { path }) => {
                assert_eq!(path, dir.path().join("config").join("loginusers.vdf"));
            }
            other => panic!("expected NoSteamUser, got {other:?}"),
        }
    }

    #[test]
    fn test_open_errors_without_steam() {
        // nicht existierendes Verzeichnis → sauberer Fehler (auch ohne Registry-Steam)
        let missing = std::env::temp_dir().join("chiaki-app-steam-does-not-exist-42");
        match SteamShortcuts::open(Some(&missing)) {
            Err(SteamError::SteamNotFound { path }) => assert_eq!(path, missing),
            other => panic!("expected SteamNotFound, got {other:?}"),
        }
        // Steam-Dir ohne loginusers.vdf → NoSteamUser
        let empty = TempDir::new("empty-steam");
        match SteamShortcuts::open(Some(empty.path())) {
            Err(SteamError::NoSteamUser { .. }) => {}
            other => panic!("expected NoSteamUser, got {other:?}"),
        }
    }

    #[test]
    fn test_find_steam_dir_fallback_and_registry_smoke() {
        // expliziter Pfad gewinnt immer
        assert_eq!(find_steam_dir(Some(Path::new("X:/Steam"))), PathBuf::from("X:/Steam"));
        // ohne Argument: Registry-Ergebnis oder C++-Fallback — nie leer
        let dir = find_steam_dir(None);
        assert!(!dir.as_os_str().is_empty());
        // Registry-Lookup selbst: Ok(SteamPath) oder sauberer Fehler, kein Panic
        match steam_path_from_registry() {
            Ok(path) => assert!(!path.as_os_str().is_empty()),
            Err(e) => debug!("no steam in registry (ok in tests): {e}"),
        }
    }

    #[test]
    fn test_library_folders() {
        let dir = fake_steam("libraries");
        let steamapps = dir.path().join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        fs::write(
            steamapps.join("libraryfolders.vdf"),
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"C:\\\\Program Files (x86)\\\\Steam\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"D:\\\\SteamLibrary\"\n\t}\n}\n",
        )
        .unwrap();
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();
        assert_eq!(
            shortcuts.library_folders().unwrap(),
            vec![
                PathBuf::from("C:\\Program Files (x86)\\Steam"),
                PathBuf::from("D:\\SteamLibrary"),
            ]
        );
    }

    // --- Add/Remove-Roundtrip im Temp-Dir ---

    #[test]
    fn test_add_update_roundtrip_in_temp_dir() {
        let dir = fake_steam("roundtrip");
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();
        assert_eq!(shortcuts.user_id(), "39734272");
        assert_eq!(
            shortcuts.shortcut_file(),
            dir.path().join("userdata").join("39734272").join("config").join("shortcuts.vdf")
        );

        // erster Add (noch keine shortcuts.vdf)
        let sc = test_shortcut("C:\\Games\\chiaki.exe", "--profile=Test");
        assert_eq!(shortcuts.add_shortcut(&sc, None).unwrap(), SteamShortcutAction::Added);

        let all = shortcuts.all_shortcuts().unwrap();
        assert_eq!(all.len(), 1);
        let entry = &all[0];
        assert_eq!(entry.app_name(), Some("Chiaki Remaster"));
        assert_eq!(entry.exe(), Some("\"C:\\Games\\chiaki.exe\""));
        assert_eq!(entry.exe_unquoted(), Some("C:\\Games\\chiaki.exe"));
        assert_eq!(entry.start_dir(), Some("C:\\Games"));
        assert_eq!(entry.launch_options(), Some("--profile=Test"));
        assert_eq!(entry.tags(), vec!["PlayStation", "Remote Play"]);
        assert_eq!(entry.app_id(), Some("2573349430"));
        assert!(shortcuts.is_installed("Chiaki Remaster", "C:\\Games\\chiaki.exe").unwrap());

        // gleiche Exe + Launch-Options → Update (kein zweiter Eintrag)
        assert_eq!(
            shortcuts.add_shortcut(&sc, None).unwrap(),
            SteamShortcutAction::Updated
        );
        assert_eq!(shortcuts.all_shortcuts().unwrap().len(), 1);

        // Backup-Datei beim Überschreiben erzeugt
        let userdata_cfg = dir.path().join("userdata").join("39734272").join("config");
        let has_backup = fs::read_dir(&userdata_cfg)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("shortcuts.vdf.") && name.ends_with(".bak")
            });
        assert!(has_backup, "expected shortcuts.vdf.<ts>.bak in {:?}", userdata_cfg);

        // andere Launch-Options → neuer Eintrag
        let sc2 = test_shortcut("C:\\Games\\chiaki.exe", "--profile=Other");
        assert_eq!(shortcuts.add_shortcut(&sc2, None).unwrap(), SteamShortcutAction::Added);
        assert_eq!(shortcuts.all_shortcuts().unwrap().len(), 2);

        // Datei selbst ist wieder gültig (Header + Roundtrip)
        let raw = fs::read(shortcuts.shortcut_file()).unwrap();
        assert!(raw.starts_with(&SHORTCUT_HEADER));
        assert_eq!(parse_shortcuts_vdf(&raw).unwrap().len(), 2);
    }

    #[test]
    fn test_add_with_artwork_writes_grid_images() {
        let dir = fake_steam("artwork");
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();
        let sc = test_shortcut("C:\\Games\\chiaki.exe", "");
        let artwork = Artwork {
            icon: Some(b"\x89PNG icon"),
            landscape: Some(b"\x89PNG landscape"),
            portrait: Some(b"\x89PNG portrait"),
            hero: Some(b"\x89PNG hero"),
            logo: Some(b"\x89PNG logo"),
        };
        shortcuts.add_shortcut(&sc, Some(&artwork)).unwrap();

        let grid = dir
            .path()
            .join("userdata")
            .join("39734272")
            .join("config")
            .join("grid");
        // shortAppId für "C:\Games\chiaki.exe" + "Chiaki Remaster"
        assert_eq!(grid_file_name("2573349430", ArtworkKind::Icon), "2573349430_icon.png");
        assert_eq!(grid_file_name("2573349430", ArtworkKind::Landscape), "2573349430.png");
        assert_eq!(grid_file_name("2573349430", ArtworkKind::Portrait), "2573349430p.png");
        assert_eq!(grid_file_name("2573349430", ArtworkKind::Hero), "2573349430_hero.png");
        assert_eq!(grid_file_name("2573349430", ArtworkKind::Logo), "2573349430_logo.png");
        for name in [
            "2573349430_icon.png",
            "2573349430.png",
            "2573349430p.png",
            "2573349430_hero.png",
            "2573349430_logo.png",
        ] {
            assert!(grid.join(name).is_file(), "missing {name}");
        }

        let entry = &shortcuts.all_shortcuts().unwrap()[0];
        assert_eq!(
            entry.icon(),
            Some(grid.join("2573349430_icon.png").to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_remove_shortcut_roundtrip() {
        let dir = fake_steam("remove");
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();
        shortcuts
            .add_shortcut(&test_shortcut("C:\\Games\\chiaki.exe", "--profile=A"), None)
            .unwrap();
        let mut other = test_shortcut("D:\\Apps\\chiaki.exe", "");
        other.app_name = "Other Game".into();
        shortcuts.add_shortcut(&other, None).unwrap();
        assert_eq!(shortcuts.all_shortcuts().unwrap().len(), 2);

        // per Exe entfernen
        assert!(shortcuts.remove_shortcut(None, Some("C:\\Games\\chiaki.exe")).unwrap());
        let rest = shortcuts.all_shortcuts().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].app_name(), Some("Other Game"));

        // per Name entfernen
        assert!(shortcuts.remove_shortcut(Some("Other Game"), None).unwrap());
        assert!(shortcuts.all_shortcuts().unwrap().is_empty());

        // nichts mehr da → Ok(false)
        assert!(!shortcuts.remove_shortcut(Some("Other Game"), None).unwrap());
        // ohne Kriterium → Fehler
        assert!(shortcuts.remove_shortcut(None, None).is_err());
        assert!(!shortcuts.is_installed("Chiaki Remaster", "C:\\Games\\chiaki.exe").unwrap());
    }

    #[test]
    fn test_controller_config_update() {
        let dir = fake_steam("controller");
        let cfg_dir = dir
            .path()
            .join("steamapps")
            .join("common")
            .join("Steam Controller Configs")
            .join("39734272")
            .join("config");
        fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_file = cfg_dir.join("configset_controller_neptune.vdf");
        fs::write(
            &cfg_file,
            "\"controller_config\"\n{\n\t\"steam big picture\"\n\t{\n\t\t\"title\"\t\t\"Steam Big Picture\"\n\t}\n}\n",
        )
        .unwrap();
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();

        shortcuts
            .update_controller_config("Chiaki Remaster", CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID)
            .unwrap();

        let raw = fs::read_to_string(&cfg_file).unwrap();
        let root = parse_vdf(raw.as_bytes()).unwrap();
        // bestehende Einträge bleiben erhalten, neuer Child mit workshop-ID
        assert_eq!(
            root.childs["steam big picture"].attribs["title"],
            "Steam Big Picture"
        );
        assert_eq!(
            root.childs["chiaki remaster"].attribs["workshop"],
            CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID
        );

        // idempotent: zweiter Lauf ändert nichts ("already set, not overwriting")
        let before = fs::read_to_string(&cfg_file).unwrap();
        shortcuts
            .update_controller_config("Chiaki Remaster", CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID)
            .unwrap();
        assert_eq!(fs::read_to_string(&cfg_file).unwrap(), before);

        // andere ID → wird überschrieben; "template" wird entfernt
        let mut entry = VdfObject::default();
        entry.name = "chiaki remaster".into();
        entry.add_attribute("template", "template_deck");
        let root = parse_vdf(before.as_bytes()).unwrap();
        let mut root = root;
        root.add_child(entry);
        fs::write(&cfg_file, write_vdf(&root)).unwrap();
        shortcuts
            .update_controller_config("Chiaki Remaster", CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID)
            .unwrap();
        let root = parse_vdf(fs::read_to_string(&cfg_file).unwrap().as_bytes()).unwrap();
        let chiaki = &root.childs["chiaki remaster"];
        assert_eq!(chiaki.attribs["workshop"], CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID);
        assert!(!chiaki.attribs.contains_key("template"));

        // Backup entstanden
        let has_backup = fs::read_dir(&cfg_dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.starts_with("configset_controller_neptune.") && name.ends_with(".bak")
            });
        assert!(has_backup);
    }

    #[test]
    fn test_controller_config_missing_file_is_not_fatal() {
        let dir = fake_steam("no-controller");
        let shortcuts = SteamShortcuts::at(dir.path()).unwrap();
        // kein Neptune-Config vorhanden → Warnung, aber Ok (C++-Verhalten)
        shortcuts
            .update_controller_config("Chiaki Remaster", CHIAKI_CONTROLLER_LAYOUT_WORKSHOP_ID)
            .unwrap();
    }

    #[test]
    fn test_backup_timestamp_format() {
        // yyyy-MM-dd-HH-mm-ss (':' wäre auf Windows im Dateinamen unzulässig),
        // deterministisch aus einer fixen Epoch-Sekunde
        let ts_days = 19_000i64; // 2022-01-08
        let (y, m, d) = civil_from_days(ts_days);
        assert_eq!((y, m, d), (2022, 1, 8));
        let (y2, m2, d2) = civil_from_days(0);
        assert_eq!((y2, m2, d2), (1970, 1, 1));
        let secs = ts_days * 86_400 + 3_661;
        let tod = (secs.rem_euclid(86_400)) as u32;
        assert_eq!(
            format!("{y:04}-{m:02}-{d:02}-{:02}-{:02}-{:02}", tod / 3600, (tod % 3600) / 60, tod % 60),
            "2022-01-08-01-01-01"
        );
    }
}
