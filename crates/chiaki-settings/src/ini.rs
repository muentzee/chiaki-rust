//! Qt-QSettings-INI-kompatibler Key/Value-Store.
//!
//! Die C++-App (chiaki-ng GUI) persistiert ihre Settings mit `QSettings` im
//! `IniFormat`. Damit die Rust-App dieselben Dateien bytekompatibel lesen UND
//! schreiben kann (User machen nur den exe-Tausch, CONVENTIONS.md Punkt 9),
//! ist hier das Dateiformat exakt nachgebaut — auf Basis von QtBase
//! `qsettings.cpp` (`iniEscapedString`, `iniUnescapedStringList`,
//! `iniEscapedKey`, `iniUnescapedKey`, `variantToString`, `stringToVariant`,
//! `writeIniFile`, `readIniFile`/`readIniSection`) und verifiziert gegen eine
//! von der C++-App erzeugte portable `data/settings.ini`.
//!
//! Formatmerkmale:
//! - Windows-Zeilenende `\r\n`.
//! - Top-Level-Keys liegen in der Sektion `[General]` (Beim Lesen wird
//!   `general` case-insensitiv als Top-Level erkannt, `[%General]` mappt auf
//!   die Gruppe `General`).
//! - Gruppen im Key werden mit `/` getrennt (intern); in der Datei wird die
//!   erste Gruppe zur Sektion, der Rest bleibt (mit `\` statt `/`) im Key.
//!   Beispiel: `registered_hosts/1/rp_key` → `[registered_hosts]` + `1\rp_key=...`.
//! - Arrays sind 1-basiert in der Datei: `beginWriteArray(prefix)` +
//!   `setArrayIndex(i)` schreibt `prefix/<i+1>/<key>` und `prefix/size`.
//! - `QByteArray` → `@ByteArray(<rohe Bytes als Latin1, escaped>)`,
//!   `QRect` → `@Rect(x y w h)`.
//! - Values werden gequotet (`"..."`), wenn sie `;`, `,` oder `=` enthalten
//!   oder mit/aus Leerzeichen enden; `\xHH`-Hex-Escapes sind lowercase und
//!   OHNE Null-Padding; folgt auf ein Hex-Escape ein Hex-Digit, wird dieses
//!   ebenfalls escaped (`escapeNextIfDigit`).

/// Repräsentiert einen QVariant-Wert im QSettings-Sinne (auf die in den
/// chiaki-Settings vorkommenden Typen reduziert).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `QVariant()` / `@Invalid()`
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    /// float/double — wie QVariant als Zahl mit `QString::number`-artiger
    /// Darstellung serialisiert (kürzeste Rundtrip-Darstellung).
    Float(f64),
    Str(String),
    ByteArray(Vec<u8>),
    /// QRect als (x, y, width, height)
    Rect(i32, i32, i32, i32),
}

impl Value {
    /// Qt `QSettingsPrivate::variantToString` (vor dem INI-Escaping).
    pub fn to_variant_string(&self) -> String {
        match self {
            Value::Null => "@Invalid()".to_string(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => qt_number(*f),
            Value::Str(s) => {
                if s.contains('\0') {
                    format!("@String({s})")
                } else if s.starts_with('@') {
                    format!("@{s}")
                } else {
                    s.clone()
                }
            }
            Value::ByteArray(a) => {
                // QByteArray wird als Latin1-"String" interpretiert;
                // die eigentlichen \xHH-Escapes passieren im ini_escape_string.
                let raw: String = a.iter().map(|&b| b as char).collect();
                format!("@ByteArray({raw})")
            }
            Value::Rect(x, y, w, h) => format!("@Rect({x} {y} {w} {h})"),
        }
    }

    /// Qt `QSettingsPrivate::stringToVariant` (nach dem INI-Unescaping).
    pub fn from_variant_string(s: &str) -> Value {
        if s.starts_with('@') {
            if s.ends_with(')') {
                if let Some(inner) = s.strip_prefix("@ByteArray(").and_then(|r| r.strip_suffix(')')) {
                    return Value::ByteArray(inner.chars().map(|c| c as u8).collect());
                }
                if let Some(inner) = s.strip_prefix("@String(").and_then(|r| r.strip_suffix(')')) {
                    return Value::Str(inner.to_string());
                }
                if let Some(inner) = s.strip_prefix("@Rect(").and_then(|r| r.strip_suffix(')')) {
                    let args = split_args(inner);
                    if args.len() == 4 {
                        if let (Some(x), Some(y), Some(w), Some(h)) = (
                            parse_i32(&args[0]),
                            parse_i32(&args[1]),
                            parse_i32(&args[2]),
                            parse_i32(&args[3]),
                        ) {
                            return Value::Rect(x, y, w, h);
                        }
                    }
                    return Value::Null;
                }
                if s == "@Invalid()" {
                    return Value::Null;
                }
            }
            if let Some(inner) = s.strip_prefix('@') {
                // Qt: QVariant(s.sliced(1)) — nur das erste '@' entfernen
                return Value::Str(inner.to_string());
            }
        }
        Value::Str(s.to_string())
    }
}

fn parse_i32(s: &str) -> Option<i32> {
    s.trim().parse::<i32>().ok()
}

/// Qt `QSettingsPrivate::splitArgs` — splittet an Leerzeichen.
fn split_args(s: &str) -> Vec<String> {
    s.split(' ').map(|p| p.to_string()).collect()
}

/// Qt `QString::number(double)`-Ersatz: Qt formatiert mit 'g' und 6
/// signifikanten Ziffern; für alle in den Settings vorkommenden Werte
/// (1–3 Nachkommastellen) liefert die kürzeste Rust-Darstellung exakt
/// denselben String (1.0 → "1", 0.3 → "0.3", 99.995 → "99.995").
fn qt_number(f: f64) -> String {
    if f.is_finite() {
        format!("{f}")
    } else {
        // Qt schreibt inf/nan via QString::number als "inf"/"nan";
        // kommt in den Settings praktisch nicht vor.
        if f.is_nan() {
            "nan".to_string()
        } else if f > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        }
    }
}

fn is_hex_digit(c: u32) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x46 | 0x61..=0x66)
}

/// Qt `QSettingsPrivate::iniEscapedString`: escaped den Variant-String für
/// die INI-Datei (inkl. Quoting-Entscheidung).
pub fn ini_escape_string(s: &str) -> String {
    // Strings, die selbst mit "@ByteArray("/"@Variant("/"@DateTime("
    // beginnen, sind binär — dort werden auch Bytes >= 0x7F escaped.
    let use_codec = !(s.starts_with("@ByteArray(")
        || s.starts_with("@Variant(")
        || s.starts_with("@DateTime("));

    let mut result = String::with_capacity(s.len() * 3 / 2 + 8);
    let mut needs_quotes = false;
    let mut escape_next_if_digit = false;

    for ch in s.chars() {
        let ch = ch as u32;
        if ch == b';' as u32 || ch == b',' as u32 || ch == b'=' as u32 {
            needs_quotes = true;
        }

        if escape_next_if_digit && is_hex_digit(ch) {
            result.push_str("\\x");
            result.push_str(&format!("{ch:x}")); // lowercase, kein Padding
            continue; // escape_next_if_digit bleibt gesetzt
        }

        escape_next_if_digit = false;

        match ch {
            0x00 => {
                result.push_str("\\0");
                escape_next_if_digit = true;
            }
            0x07 => result.push_str("\\a"),
            0x08 => result.push_str("\\b"),
            0x0C => result.push_str("\\f"),
            0x0A => result.push_str("\\n"),
            0x0D => result.push_str("\\r"),
            0x09 => result.push_str("\\t"),
            0x0B => result.push_str("\\v"),
            0x22 | 0x5C => {
                // '"' und '\'
                result.push('\\');
                result.push(ch as u8 as char);
            }
            _ => {
                if ch <= 0x1F || (ch >= 0x7F && !use_codec) {
                    result.push_str("\\x");
                    result.push_str(&format!("{ch:x}")); // lowercase, kein Padding
                    escape_next_if_digit = true;
                } else {
                    result.push(char::from_u32(ch).unwrap_or('\u{FFFD}'));
                }
            }
        }
    }

    let first_is_space = result.starts_with(' ');
    let last_is_space = result.ends_with(' ');
    if needs_quotes || (!result.is_empty() && (first_is_space || last_is_space)) {
        result.insert(0, '"');
        result.push('"');
    }
    result
}

/// Qt `QSettingsPrivate::iniUnescapedStringList` (nur Single-String-Fall;
/// Stringlisten kommen in den chiaki-Settings nicht vor).
/// Liefert den dekodierten String.
pub fn ini_unescape_string(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                let c = bytes[i];
                i += 1;
                match c {
                    b'a' => result.push('\u{07}'),
                    b'b' => result.push('\u{08}'),
                    b'f' => result.push('\u{0C}'),
                    b'n' => result.push('\n'),
                    b'r' => result.push('\r'),
                    b't' => result.push('\t'),
                    b'v' => result.push('\u{0B}'),
                    b'"' => result.push('"'),
                    b'?' => result.push('?'),
                    b'\'' => result.push('\''),
                    b'\\' => result.push('\\'),
                    b'x' => {
                        if i < bytes.len() && is_hex_digit(bytes[i] as u32) {
                            let mut val: u32 = 0;
                            while i < bytes.len() && is_hex_digit(bytes[i] as u32) {
                                val = (val << 4) | hex_val(bytes[i]);
                                i += 1;
                            }
                            result.push(char::from_u32(val & 0xFFFF).unwrap_or('\u{FFFD}'));
                        }
                    }
                    b'0'..=b'7' => {
                        let mut val: u32 = (c - b'0') as u32;
                        while i < bytes.len() && (b'0'..=b'7').contains(&bytes[i]) {
                            val = (val << 3) | (bytes[i] - b'0') as u32;
                            i += 1;
                        }
                        result.push(char::from_u32(val & 0xFFFF).unwrap_or('\u{FFFD}'));
                    }
                    b'\n' | b'\r' => {
                        // \n, \r, \r\n und \n\r sind Zeilenfortsetzungen
                        if i < bytes.len() && (bytes[i] == b'\n' || bytes[i] == b'\r') && bytes[i] != c {
                            i += 1;
                        }
                    }
                    _ => {
                        // Zeichen wird übersprungen (wie in Qt)
                    }
                }
            }
            b'"' => {
                i += 1;
            }
            _ => {
                // bis zum nächsten Backslash, Quote oder Komma kopieren;
                // Kommas (Stringlisten) hier wie Normalzeichen behandeln.
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'\\' && bytes[j] != b'"' {
                    j += 1;
                }
                result.push_str(&String::from_utf8_lossy(&bytes[i..j]));
                i = j;
            }
        }
    }
    result
}

fn hex_val(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => (c - b'0') as u32,
        b'a'..=b'f' => (c - b'a' + 10) as u32,
        b'A'..=b'F' => (c - b'A' + 10) as u32,
        _ => 0,
    }
}

/// Qt `QSettingsPrivate::iniEscapedKey` (Schreibrichtung).
/// `/` wird zu `\`, erlaubte Zeichen bleiben literal, alles andere wird als
/// `%XX` (bzw. `%UXXXX`) UPPERCASE-hex kodiert.
pub fn ini_escape_key(key: &str) -> String {
    let mut result = String::new();
    for ch in key.chars() {
        let c = ch as u32;
        if ch == '/' {
            result.push('\\');
        } else if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            result.push(ch);
        } else if c <= 0xFF {
            result.push('%');
            result.push_str(&format!("{c:02X}"));
        } else {
            result.push_str("%U");
            result.push_str(&format!("{c:04X}"));
        }
    }
    result
}

/// Qt `QSettingsPrivate::iniUnescapedKey` (Lesrichtung): `\` → `/`,
/// `%XX`/`%UXXXX` dekodieren.
pub fn ini_unescape_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let size = chars.len();
    let mut result = String::new();
    let mut i = 0usize;
    while i < size {
        let ch = chars[i];
        if ch == '\\' {
            result.push('/');
            i += 1;
            continue;
        }
        if ch != '%' || i == size - 1 {
            result.push(ch);
            i += 1;
            continue;
        }
        let mut num_digits = 2usize;
        let mut first_digit_pos = i + 1;
        if chars[i + 1] == 'U' {
            first_digit_pos += 1;
            num_digits = 4;
        }
        if first_digit_pos + num_digits > size {
            result.push('%');
            i += 1;
            continue;
        }
        let hex: String = chars[first_digit_pos..first_digit_pos + num_digits].iter().collect();
        match u32::from_str_radix(&hex, 16) {
            Ok(v) => {
                result.push(char::from_u32(v).unwrap_or('\u{FFFD}'));
                i = first_digit_pos + num_digits;
            }
            Err(_) => {
                result.push('%');
                i += 1;
            }
        }
    }
    result
}

/// Store, der die Einträge einer QSettings-INI-Datei in Einfügereihenfolge
/// hält. Keys sind "volle" QSettings-Keys mit `/` als Gruppentrenner
/// (z.B. `settings/audio_volume`, `registered_hosts/1/rp_key`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IniStore {
    entries: Vec<(String, String)>, // (full key, RAW escaped value)
}

impl IniStore {
    pub fn new() -> Self {
        IniStore { entries: Vec::new() }
    }

    pub fn from_entries(entries: Vec<(String, String)>) -> Self {
        IniStore { entries }
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    /// Qt `readIniFile` + `readIniSection` — parst eine QSettings-INI.
    /// Parse-Fehler einzelner Zeilen werden toleriert (wie in Qt: so viel
    /// wie möglich lesen).
    pub fn parse(text: &str) -> IniStore {
        let mut store = IniStore::new();
        let mut section = String::new(); // mit führendem '/'? nein: Gruppenname ohne '/'

        // UTF-8-BOM überspringen
        let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);

        for line in split_lines(text) {
            let line = strip_inline_comment(line);
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                let inner = match line.find(']') {
                    Some(idx) => line[1..idx].trim(),
                    None => line[1..].trim(),
                };
                if inner.eq_ignore_ascii_case("general") {
                    section.clear();
                } else if let Some(stripped) = inner.strip_prefix('%') {
                    if stripped.eq_ignore_ascii_case("general") {
                        section = "General".to_string();
                    } else {
                        section = ini_unescape_key(stripped);
                    }
                } else {
                    section = ini_unescape_key(inner);
                }
                continue;
            }

            // erstes '=' außerhalb von Quotes finden
            let Some(eq) = find_unquoted_equals(line) else {
                continue;
            };
            let key_raw = line[..eq].trim();
            let value_raw = &line[eq + 1..];
            if key_raw.is_empty() && section.is_empty() {
                continue;
            }
            let key_dec = ini_unescape_key(key_raw);
            let full_key = if section.is_empty() {
                key_dec
            } else {
                format!("{section}/{key_dec}")
            };
            // ROHWERT speichern (inkl. Quotes/Escapes), damit ein unangetastetes
            // Re-Save die Datei byte-identisch lässt; dekodiert wird on demand.
            store.set_raw(full_key, value_raw.to_string());
        }
        store
    }

    /// Qt `writeIniFile` — serialisiert in das exakte QSettings-INI-Format
    /// (CRLF, `[General]`-Sektion, Einfügereihenfolge).
    pub fn to_ini_string(&self) -> String {
        const EOL: &str = "\r\n";
        let mut out = String::new();

        // Sektionen in Einfügereihenfolge (Position des ersten Keys) gruppieren
        let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for (key, raw) in &self.entries {
            let (section, rest) = match key.find('/') {
                Some(pos) => (key[..pos].to_string(), key[pos + 1..].to_string()),
                None => (String::new(), key.clone()),
            };
            match sections.iter_mut().find(|(s, _)| *s == section) {
                Some((_, keys)) => keys.push((rest, raw.clone())),
                None => sections.push((section, vec![(rest, raw.clone())])),
            }
        }

        for (i, (section, keys)) in sections.iter().enumerate() {
            let mut header = if section.is_empty() {
                "[General]".to_string()
            } else if section.eq_ignore_ascii_case("general") {
                "[%General]".to_string()
            } else {
                format!("[{}]", ini_escape_key(section))
            };
            if i != 0 {
                header.insert_str(0, EOL);
            }
            header.push_str(EOL);
            out.push_str(&header);

            for (rest, raw) in keys {
                out.push_str(&ini_escape_key(rest));
                out.push('=');
                out.push_str(raw);
                out.push_str(EOL);
            }
        }
        out
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    pub fn get_raw(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn set_raw(&mut self, key: String, raw: String) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = raw,
            None => self.entries.push((key, raw)),
        }
    }

    /// Wie `QSettings::setValue` (Wert bleibt in Einfügeposition).
    pub fn set_value(&mut self, key: &str, value: Value) {
        let raw = ini_escape_string(&value.to_variant_string());
        self.set_raw(key.to_string(), raw);
    }

    /// Setzt einen bereits escapteren Rohwert (für bytegenaues Kopieren
    /// zwischen Stores, z.B. Export/Import und Migrationen).
    pub fn set_raw_value(&mut self, key: &str, raw: String) {
        self.set_raw(key.to_string(), raw);
    }

    /// Wie `QSettings::remove(key)` — entfernt Key und, wenn es eine Gruppe
    /// ist, alle darunterliegenden Keys.
    pub fn remove(&mut self, key: &str) {
        let prefix = format!("{key}/");
        self.entries.retain(|(k, _)| k != key && !k.starts_with(&prefix));
    }

    pub fn remove_prefix(&mut self, prefix: &str) {
        self.entries.retain(|(k, _)| !k.starts_with(prefix));
    }

    pub fn keys(&self) -> Vec<&str> {
        self.entries.iter().map(|(k, _)| k.as_str()).collect()
    }

    // ---------- Convenience-Getter (QVariant-Semantik) ----------

    fn value(&self, key: &str) -> Option<Value> {
        let raw = self.get_raw(key)?;
        let decoded = ini_unescape_string(raw);
        Some(Value::from_variant_string(&decoded))
    }

    /// `settings.value(key, default).toString()`
    pub fn string_or(&self, key: &str, default: &str) -> String {
        match self.value(key) {
            Some(Value::Str(s)) => s,
            Some(Value::ByteArray(a)) => a.iter().map(|&b| b as char).collect(),
            Some(_) => String::new(),
            None => default.to_string(),
        }
    }

    /// `settings.value(key, default).toBool()`
    pub fn bool_or(&self, key: &str, default: bool) -> bool {
        match self.value(key) {
            Some(Value::Bool(b)) => b,
            Some(Value::Int(i)) => i != 0,
            Some(Value::UInt(u)) => u != 0,
            Some(Value::Str(s)) => qstring_to_bool(&s),
            Some(_) => false,
            None => default,
        }
    }

    /// `settings.value(key, default).toInt()`
    pub fn int_or(&self, key: &str, default: i64) -> i64 {
        match self.value(key) {
            Some(Value::Int(i)) => i,
            Some(Value::UInt(u)) => u as i64,
            Some(Value::Bool(b)) => i64::from(b),
            Some(Value::Str(s)) => s.trim().parse::<i64>().unwrap_or(0),
            Some(_) => 0,
            None => default,
        }
    }

    /// `settings.value(key, default).toUInt()`
    pub fn uint_or(&self, key: &str, default: u64) -> u64 {
        match self.value(key) {
            Some(Value::UInt(u)) => u,
            Some(Value::Int(i)) if i >= 0 => i as u64,
            Some(Value::Str(s)) => s.trim().parse::<u64>().unwrap_or(0),
            Some(_) => 0,
            None => default,
        }
    }

    /// `settings.value(key, default).toFloat()`
    pub fn float_or(&self, key: &str, default: f64) -> f64 {
        match self.value(key) {
            Some(Value::Float(f)) => f,
            Some(Value::Int(i)) => i as f64,
            Some(Value::UInt(u)) => u as f64,
            Some(Value::Str(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
            Some(_) => 0.0,
            None => default,
        }
    }

    /// `settings.value(key).toByteArray()`
    pub fn byte_array(&self, key: &str) -> Option<Vec<u8>> {
        match self.value(key) {
            Some(Value::ByteArray(a)) => Some(a),
            Some(Value::Str(s)) => Some(s.bytes().collect()),
            _ => None,
        }
    }

    /// `settings.value(key, QRect()).toRect()`
    pub fn rect_or(&self, key: &str) -> Option<(i32, i32, i32, i32)> {
        match self.value(key) {
            Some(Value::Rect(x, y, w, h)) => Some((x, y, w, h)),
            _ => None,
        }
    }

    // ---------- Array-Zugriff (QSettings beginReadArray/beginWriteArray) ----------

    /// Liest `prefix/size` und liefert für jeden 1-basierten Eintrag die
    /// (Subkey, Value)-Paare.
    pub fn read_array(&self, prefix: &str) -> Vec<Vec<(String, Value)>> {
        let size = self.int_or(&format!("{prefix}/size"), 0).max(0) as usize;
        let mut result = Vec::with_capacity(size.min(4096));
        for n in 1..=size {
            let mut item = Vec::new();
            let entry_prefix = format!("{prefix}/{n}/");
            for (k, raw) in &self.entries {
                if let Some(sub) = k.strip_prefix(&entry_prefix) {
                    let decoded = ini_unescape_string(raw);
                    item.push((sub.to_string(), Value::from_variant_string(&decoded)));
                }
            }
            result.push(item);
        }
        result
    }

    /// Schreibt ein Array: entfernt alle `prefix/...`-Keys (inkl. veralteter
    /// Einträge über der neuen Größe), setzt `prefix/size` und die Einträge
    /// 1-basiert.
    pub fn write_array<I>(&mut self, prefix: &str, items: I)
    where
        I: IntoIterator<Item = Vec<(String, Value)>>,
    {
        let p = format!("{prefix}/");
        self.entries.retain(|(k, _)| !k.starts_with(&p));
        let items: Vec<_> = items.into_iter().collect();
        self.set_value(&format!("{prefix}/size"), Value::Int(items.len() as i64));
        for (i, item) in items.iter().enumerate() {
            for (sub, value) in item {
                self.set_value(&format!("{prefix}/{}/{}", i + 1, sub), value.clone());
            }
        }
    }
}

fn qstring_to_bool(s: &str) -> bool {
    // QVariant-String→bool-Konvertierung, ausreichend exakt für QSettings-Werte
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    if t.eq_ignore_ascii_case("false") {
        return false;
    }
    if t.eq_ignore_ascii_case("true") {
        return true;
    }
    t.parse::<i64>().map(|v| v != 0).unwrap_or(true)
}

fn find_unquoted_equals(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b'=' if !in_quotes => return Some(i),
            _ => {}
        }
    }
    None
}

/// Zeilen splitten ('\n' bzw. '\r\n'/'\r') und CR entfernen.
fn split_lines(text: &str) -> Vec<&str> {
    text.split(['\n', '\r']).filter(|l| !l.is_empty()).collect()
}

/// Zeilenkommentare: '#' oder ';' am Zeilenanfang beendet die Zeile;
/// '#'/';' außerhalb von Quotes beendet die Zeile ebenfalls (wie readIniLine).
fn strip_inline_comment(line: &str) -> &str {
    let trimmed_start = line.trim_start();
    if trimmed_start.starts_with('#') || trimmed_start.starts_with(';') {
        return "";
    }
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, b) in line.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_quotes => escaped = true,
            b'"' => in_quotes = !in_quotes,
            b'#' | b';' if !in_quotes => return &line[..i],
            _ => {}
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An die echte C++-QSettings-Ausgabe angelehnte Fixture (Struktur der
    /// portablen `data/settings.ini` aus chiaki-remaster-Win, mit
    /// synthetischen Daten statt echter User-Daten).
    const REFERENCE_INI: &str = "[General]\r\n\
        version=2\r\n\
        \r\n\
        [registered_hosts]\r\n\
        1\\ap_bssid=3132333435\r\n\
        1\\ap_key=\r\n\
        1\\ap_name=PS5\r\n\
        1\\ap_ssid=\r\n\
        1\\console_pin=0\r\n\
        1\\rp_key=@ByteArray(0\\xb0\\x42H~\\xc9&\\x9a\\xf2\\x18\\x93\\xf6\\xb0Ud\\x92)\r\n\
        1\\rp_key_type=2\r\n\
        1\\rp_regist_key=@ByteArray(7c3e91a4\\0\\0\\0\\0\\0\\0\\0\\0)\r\n\
        1\\server_mac=@ByteArray(\\xd4\\xf7\\xd5\\x11\\xfa\\x45)\r\n\
        1\\server_nickname=Test-PS5\r\n\
        1\\target=1000100\r\n\
        size=1\r\n\
        \r\n\
        [settings]\r\n\
        automatic_connect=true\r\n\
        nv_vsr=true\r\n\
        nv_vsr_scale=200\r\n\
        psn_account_id=\"eVr/5uFEAHE=\"\r\n\
        psn_auth_token=test-auth-token\r\n\
        psn_refresh_token=test-refresh-token\r\n\
        resolution_local_ps5=1080p\r\n\
        stream_geometry=@Rect(0 23 3072 1705)\r\n\
        window_type=Adjust Manually\r\n\
        \r\n\
        [controller_mappings]\r\n\
        size=0\r\n";

    #[test]
    fn reference_ini_roundtrip_byte_identical() {
        let store = IniStore::parse(REFERENCE_INI);
        assert_eq!(store.int_or("version", 0), 2);
        assert_eq!(store.string_or("settings/resolution_local_ps5", ""), "1080p");
        assert_eq!(
            store.byte_array("registered_hosts/1/server_mac"),
            Some(vec![0xd4, 0xf7, 0xd5, 0x11, 0xfa, 0x45])
        );
        assert_eq!(
            store.byte_array("registered_hosts/1/rp_regist_key"),
            Some(b"7c3e91a4\0\0\0\0\0\0\0\0".to_vec())
        );
        assert_eq!(
            store.byte_array("registered_hosts/1/rp_key"),
            Some(vec![
                0x30, 0xb0, 0x42, 0x48, 0x7e, 0xc9, 0x26, 0x9a, 0xf2, 0x18, 0x93, 0xf6, 0xb0, 0x55,
                0x64, 0x92
            ])
        );
        assert_eq!(store.uint_or("registered_hosts/1/rp_key_type", 0), 2);
        assert_eq!(store.int_or("registered_hosts/size", 0), 1);
        assert_eq!(
            store.rect_or("settings/stream_geometry"),
            Some((0, 23, 3072, 1705))
        );
        assert_eq!(store.string_or("settings/psn_account_id", ""), "eVr/5uFEAHE=");
        assert_eq!(store.bool_or("settings/automatic_connect", false), true);
        assert_eq!(store.read_array("controller_mappings").len(), 0);

        // Re-Serialisierung muss byte-identisch zur C++-Ausgabe sein
        assert_eq!(store.to_ini_string(), REFERENCE_INI);
    }

    #[test]
    fn escape_hex_digit_followup() {
        // Nach einem \xHH-Escape wird ein folgendes Hex-Digit ebenfalls escaped
        // (0x30 '0', 0xb0, 0x42 'B' → 0\xb0\x42), danach normale Literale.
        let v = Value::ByteArray(vec![0x30, 0xb0, 0x42, 0x48, 0x7e, 0xc9]);
        assert_eq!(v.to_variant_string(), "@ByteArray(0\u{b0}BH~\u{c9})");
        assert_eq!(ini_escape_string(&v.to_variant_string()), "@ByteArray(0\\xb0\\x42H~\\xc9)");
    }

    #[test]
    fn escape_nul_and_padding() {
        // NUL → \0, danach \0 wieder \0 (Hex-Digit-Folge gilt auch hier)
        let v = Value::ByteArray(b"7c3e91a4\0\0\0\0\0\0\0\0".to_vec());
        assert_eq!(
            ini_escape_string(&v.to_variant_string()),
            "@ByteArray(7c3e91a4\\0\\0\\0\\0\\0\\0\\0\\0)"
        );
    }

    #[test]
    fn quoting_rules() {
        // '=' triggert Quoting, wird aber selbst nicht escaped
        assert_eq!(ini_escape_string("eVr/5uFEAHE="), "\"eVr/5uFEAHE=\"");
        // führendes/abschließendes Leerzeichen triggert Quoting
        assert_eq!(ini_escape_string(" a "), "\" a \"");
        // normale Strings bleiben unquotiert
        assert_eq!(ini_escape_string("Adjust Manually"), "Adjust Manually");
        // kleine Hex-Werte ohne Padding (\x9, nicht \x09)
        assert_eq!(ini_escape_string("a\u{09}"), "a\\t");
        assert_eq!(ini_escape_string("a\u{05}"), "a\\x5");
    }

    #[test]
    fn bool_int_float_strings() {
        let mut s = IniStore::new();
        s.set_value("a", Value::Bool(true));
        s.set_value("b", Value::Bool(false));
        s.set_value("c", Value::Int(-42));
        s.set_value("d", Value::UInt(28000));
        s.set_value("e", Value::Float(0.3));
        s.set_value("f", Value::Float(99.995));
        assert_eq!(s.bool_or("a", false), true);
        assert_eq!(s.bool_or("b", true), false);
        assert_eq!(s.int_or("c", 0), -42);
        assert_eq!(s.uint_or("d", 0), 28000);
        assert_eq!(s.float_or("e", 0.0), 0.3);
        assert_eq!(s.get_raw("e"), Some("0.3"));
        assert_eq!(s.float_or("f", 0.0), 99.995);
        assert_eq!(s.bool_or("zz", true), true);
        assert_eq!(s.int_or("zz", 17), 17);
    }

    #[test]
    fn rect_roundtrip() {
        let mut s = IniStore::new();
        s.set_value("g", Value::Rect(201, 142, 1775, 1314));
        assert_eq!(s.get_raw("g"), Some("@Rect(201 142 1775 1314)"));
        assert_eq!(s.rect_or("g"), Some((201, 142, 1775, 1314)));
    }

    #[test]
    fn at_string_prefix_is_doubled() {
        let mut s = IniStore::new();
        s.set_value("k", Value::Str("@foo".to_string()));
        // Qt verdoppelt nur das @ im Variant-String, quotiert aber nicht
        // (nur ; , = und Rand-Spaces triggern Quoting)
        assert_eq!(s.get_raw("k"), Some("@@foo"));
        assert_eq!(s.string_or("k", ""), "@foo");
    }

    #[test]
    fn array_write_read() {
        let mut s = IniStore::new();
        s.write_array(
            "registered_hosts",
            vec![vec![
                ("server_nickname".to_string(), Value::Str("A".into())),
                ("target".to_string(), Value::Int(1000)),
            ]],
        );
        let arr = s.read_array("registered_hosts");
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0].iter().find(|(k, _)| k == "server_nickname").unwrap().1,
            Value::Str("A".into())
        );
        // Array neu schreiben mit weniger Einträgen entfernt alte Keys
        s.write_array(
            "registered_hosts",
            vec![
                vec![("server_nickname".to_string(), Value::Str("B".into()))],
                vec![("server_nickname".to_string(), Value::Str("C".into()))],
            ],
        );
        assert_eq!(s.read_array("registered_hosts").len(), 2);
        s.write_array("registered_hosts", vec![vec![(
            "server_nickname".to_string(),
            Value::Str("D".into()),
        )]]);
        let out = s.to_ini_string();
        assert!(!out.contains("server_nickname=B"));
        assert!(!out.contains("server_nickname=C"));
        assert!(out.contains("1\\server_nickname=D"));
        assert!(out.contains("size=1"));
    }

    #[test]
    fn remove_group_removes_children() {
        let mut s = IniStore::new();
        s.set_value("settings/a", Value::Int(1));
        s.set_value("settings/b", Value::Int(2));
        s.set_value("other/c", Value::Int(3));
        s.remove("settings");
        assert!(!s.contains("settings/a"));
        assert!(!s.contains("settings/b"));
        assert!(s.contains("other/c"));
    }

    #[test]
    fn escaped_keys_percent_decoding() {
        let ini = "[General]\r\na%20b=1\r\n[sect%20ion]\r\nx=2\r\n";
        let s = IniStore::parse(ini);
        assert_eq!(s.int_or("a b", 0), 1);
        assert_eq!(s.int_or("sect ion/x", 0), 2);
    }
}
