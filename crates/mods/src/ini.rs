//! A line-preserving ini patcher, driven by a per-mod safe surface.
//!
//! Every mod system that ships an `OptiScaler.ini` (Tier B OptiScaler, HelixSR)
//! or an `dlssnr-amd.ini` (DLSSNR-AMD) wants to change *a few* values and leave
//! the rest exactly as upstream wrote it. Upstream rewrites these files between
//! releases and the in-game menu owns everything else, so the launcher must not
//! reformat, reorder or drop a comment — it must only replace the values it is
//! explicitly allowed to touch (ADR-0024).
//!
//! The rule is data, not code: an [`IniSurface`] lists the sections and keys a
//! mod may set and the values each key accepts. [`IniSurface::patch`] refuses an
//! unknown section, an unknown key or an out-of-range value by name, and
//! otherwise rewrites only the value token of the matching line — indentation,
//! comments (whole-line and inline), blank lines and ordering all survive.
//!
//! Framework-free and pure: `patch` takes a string and returns a string, so the
//! caller decides where a profile file lives.

/// The values one key is allowed to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedValues {
    /// Any value is accepted (a free-form path, a model name).
    Any,
    /// One of a fixed, case-insensitive set (a boolean, an enum).
    OneOf(Vec<String>),
    /// An integer inside an inclusive range (`model_scale`).
    Range {
        /// The lowest accepted value.
        min: i64,
        /// The highest accepted value.
        max: i64,
    },
}

impl AllowedValues {
    /// A fixed set from string literals.
    pub fn one_of<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::OneOf(values.into_iter().map(Into::into).collect())
    }

    /// The boolean pair `true`/`false`.
    pub fn boolean() -> Self {
        Self::one_of(["true", "false"])
    }

    /// Whether `value` is acceptable, and why not when it is not.
    fn check(&self, value: &str) -> Result<(), String> {
        match self {
            Self::Any => Ok(()),
            Self::OneOf(values) => {
                if values
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(value))
                {
                    Ok(())
                } else {
                    Err(format!("expected one of {}", values.join(", ")))
                }
            }
            Self::Range { min, max } => match value.trim().parse::<i64>() {
                Ok(number) if (*min..=*max).contains(&number) => Ok(()),
                Ok(_) => Err(format!("expected an integer in {min}..={max}")),
                Err(_) => Err(format!("expected an integer in {min}..={max}")),
            },
        }
    }
}

/// One key a mod may set, and the values it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceKey {
    /// The key name, as it appears before `=`.
    pub name: String,
    /// The accepted values.
    pub allowed: AllowedValues,
}

impl SurfaceKey {
    /// A key with its allowed values.
    pub fn new(name: impl Into<String>, allowed: AllowedValues) -> Self {
        Self {
            name: name.into(),
            allowed,
        }
    }
}

/// One section a mod may write into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceSection {
    /// The section name, without brackets.
    pub name: String,
    /// The keys this mod may set inside it.
    pub keys: Vec<SurfaceKey>,
}

impl SurfaceSection {
    /// A section with its allowed keys.
    pub fn new(name: impl Into<String>, keys: impl IntoIterator<Item = SurfaceKey>) -> Self {
        Self {
            name: name.into(),
            keys: keys.into_iter().collect(),
        }
    }

    /// The key with this name, if the surface allows it.
    pub fn key(&self, name: &str) -> Option<&SurfaceKey> {
        self.keys.iter().find(|key| key.name == name)
    }
}

/// The safe surface for one mod: the only sections and keys it may set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniSurface {
    /// The mod's name, for a message.
    pub name: String,
    /// The allowed sections.
    pub sections: Vec<SurfaceSection>,
}

impl IniSurface {
    /// A surface for `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sections: Vec::new(),
        }
    }

    /// Add a section, for building a surface fluently.
    #[must_use]
    pub fn section(mut self, section: SurfaceSection) -> Self {
        self.sections.push(section);
        self
    }

    /// The allowed section with this name, if any.
    pub fn allowed_section(&self, name: &str) -> Option<&SurfaceSection> {
        self.sections.iter().find(|section| section.name == name)
    }

    /// The Tier B OptiScaler `OptiScaler.ini` surface.
    ///
    /// Only the upscaler choice and the frame-gen toggle are exposed; the rest
    /// of the file is the in-game menu's (ADR-0024).
    pub fn optiscaler() -> Self {
        Self::new("optiscaler")
            .section(SurfaceSection::new(
                "Upscalers",
                [SurfaceKey::new("Dx12Upscaler", AllowedValues::Any)],
            ))
            .section(SurfaceSection::new(
                "FrameGen",
                [
                    SurfaceKey::new("Enabled", AllowedValues::boolean()),
                    SurfaceKey::new("FGType", AllowedValues::Any),
                ],
            ))
    }

    /// The HelixSR surface: `[HelixSR] Int8` and `WaveSize`.
    pub fn helixsr() -> Self {
        Self::new("helixsr").section(SurfaceSection::new(
            "HelixSR",
            [
                SurfaceKey::new("Int8", AllowedValues::one_of(["auto", "on", "off"])),
                SurfaceKey::new("WaveSize", AllowedValues::one_of(["auto", "64", "32"])),
            ],
        ))
    }

    /// The DLSSNR-AMD surface (the `optiscaler` route).
    pub fn dlssnr() -> Self {
        Self::new("dlssnr")
            .section(SurfaceSection::new(
                "Network",
                [
                    SurfaceKey::new("Model", AllowedValues::Any),
                    SurfaceKey::new("model_scale", AllowedValues::Range { min: 25, max: 100 }),
                ],
            ))
            .section(SurfaceSection::new(
                "Int4Mixed",
                [SurfaceKey::new("Enabled", AllowedValues::boolean())],
            ))
            .section(SurfaceSection::new(
                "Preprocess",
                [
                    SurfaceKey::new("Enabled", AllowedValues::boolean()),
                    SurfaceKey::new("Exposure", AllowedValues::Any),
                    SurfaceKey::new("Curve", AllowedValues::Any),
                ],
            ))
            .section(SurfaceSection::new(
                "NRControl",
                [SurfaceKey::new("MenuKey", AllowedValues::Any)],
            ))
    }

    /// Refuse a section or key that is not on the surface, and return the key.
    pub fn allow(&self, section: &str, key: &str) -> Result<&SurfaceKey, IniError> {
        let section = self
            .allowed_section(section)
            .ok_or_else(|| IniError::UnknownSection {
                surface: self.name.clone(),
                section: section.to_string(),
            })?;
        section.key(key).ok_or_else(|| IniError::UnknownKey {
            surface: self.name.clone(),
            section: section.name.clone(),
            key: key.to_string(),
        })
    }

    /// Validate one assignment without applying it.
    pub fn validate(&self, setting: &IniSetting) -> Result<(), IniError> {
        let key = self.allow(&setting.section, &setting.key)?;
        key.allowed
            .check(&setting.value)
            .map_err(|reason| IniError::InvalidValue {
                section: setting.section.clone(),
                key: setting.key.clone(),
                value: setting.value.clone(),
                reason,
            })
    }

    /// The current value of an allowed key, or `None` if the file does not set
    /// it. An unknown section or key is refused by name.
    pub fn read(&self, text: &str, section: &str, key: &str) -> Result<Option<String>, IniError> {
        self.allow(section, key)?;
        let mut current: Option<String> = None;
        for line in text.split('\n') {
            if let Some(name) = section_header(line) {
                current = Some(name.to_string());
                continue;
            }
            if current.as_deref() != Some(section) {
                continue;
            }
            if let Some(parsed) = key_line(line)
                && parsed.key == key
            {
                return Ok(Some(parsed.value()));
            }
        }
        Ok(None)
    }

    /// Every allowed key this file sets, in file order.
    pub fn current(&self, text: &str) -> Vec<IniValue> {
        let mut values = Vec::new();
        let mut current: Option<String> = None;
        for line in text.split('\n') {
            if let Some(name) = section_header(line) {
                current = Some(name.to_string());
                continue;
            }
            let Some(section) = current.as_deref() else {
                continue;
            };
            let Some(parsed) = key_line(line) else {
                continue;
            };
            if self
                .allowed_section(section)
                .is_some_and(|surface| surface.key(&parsed.key).is_some())
            {
                values.push(IniValue {
                    section: section.to_string(),
                    key: parsed.key.clone(),
                    value: parsed.value(),
                });
            }
        }
        values
    }

    /// Apply every setting, line-preserving, or refuse the whole patch.
    ///
    /// Every setting is validated against the surface *before* any is applied,
    /// so a refused patch cannot half-modify a file. An existing line's value
    /// token is replaced in place (comments, indentation, blank lines and order
    /// survive); a missing key is added to the end of its section, and a missing
    /// section is appended to the end of the file.
    pub fn patch(&self, text: &str, settings: &[IniSetting]) -> Result<String, IniError> {
        for setting in settings {
            self.validate(setting)?;
        }
        let mut lines: Vec<String> = text.split('\n').map(ToString::to_string).collect();
        for setting in settings {
            if let Some(index) = find_key_line(&lines, &setting.section, &setting.key) {
                lines[index] = replace_value(&lines[index], &setting.value);
            } else {
                insert_key(&mut lines, &setting.section, &setting.key, &setting.value);
            }
        }
        Ok(lines.join("\n"))
    }
}

/// One `section.key=value` setting a caller wants to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniSetting {
    /// The section name, without brackets.
    pub section: String,
    /// The key name.
    pub key: String,
    /// The value to write.
    pub value: String,
}

impl IniSetting {
    /// A setting.
    pub fn new(
        section: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            section: section.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    /// Parse `Section.Key=Value`.
    pub fn parse(text: &str) -> Result<Self, IniError> {
        let (left, value) = text
            .split_once('=')
            .ok_or_else(|| IniError::MalformedSetting {
                text: text.to_string(),
                reason: "expected Section.Key=Value".to_string(),
            })?;
        let (section, key) = left
            .split_once('.')
            .ok_or_else(|| IniError::MalformedSetting {
                text: text.to_string(),
                reason: "expected a `Section.Key` before `=`".to_string(),
            })?;
        if section.is_empty() || key.is_empty() {
            return Err(IniError::MalformedSetting {
                text: text.to_string(),
                reason: "the section and key must not be empty".to_string(),
            });
        }
        Ok(Self::new(section, key, value))
    }
}

/// One allowed key and the value it currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniValue {
    /// The section it lives in.
    pub section: String,
    /// The key name.
    pub key: String,
    /// Its current value.
    pub value: String,
}

/// The section name of a `[Name]` line, without the brackets.
fn section_header(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('[')?;
    let close = inner.find(']')?;
    Some(inner[..close].trim())
}

/// A parsed key line: the key name and the byte span of its value token.
struct KeyLine {
    key: String,
    /// The whole line with its line ending trimmed, so spans index it.
    body: String,
    /// The start of the value token (after `=` and any spaces).
    value_start: usize,
    /// The end of the value token (before trailing spaces/comment).
    value_end: usize,
}

impl KeyLine {
    /// The value this line sets.
    fn value(&self) -> String {
        self.body[self.value_start..self.value_end].to_string()
    }
}

/// Parse a key line, or `None` for a blank, comment or section line.
fn key_line(line: &str) -> Option<KeyLine> {
    let body = line.strip_suffix('\r').unwrap_or(line);
    let trimmed = body.trim_start();
    if trimmed.is_empty()
        || trimmed.starts_with(';')
        || trimmed.starts_with('#')
        || trimmed.starts_with('[')
    {
        return None;
    }
    let equals = body.find('=')?;
    let key = body[..equals].trim();
    if key.is_empty() || key.contains('[') || key.contains(']') {
        return None;
    }
    let after = &body[equals + 1..];
    let value_start = equals + 1 + (after.len() - after.trim_start().len());
    let rest = &body[value_start..];
    let comment = rest
        .find([';', '#'])
        .filter(|pos| *pos == 0 || rest[..*pos].ends_with(char::is_whitespace));
    let value_end = value_start
        + match comment {
            Some(pos) => rest[..pos].trim_end().len(),
            None => rest.trim_end().len(),
        };
    Some(KeyLine {
        key: key.to_string(),
        body: body.to_string(),
        value_start,
        value_end,
    })
}

/// Replace the value token of a key line, keeping everything else — including a
/// trailing `\r`, so a CRLF file stays CRLF.
fn replace_value(line: &str, value: &str) -> String {
    let Some(parsed) = key_line(line) else {
        return line.to_string();
    };
    let cr = if line.ends_with('\r') { "\r" } else { "" };
    let body = &parsed.body;
    format!(
        "{}{}{}{cr}",
        &body[..parsed.value_start],
        value,
        &body[parsed.value_end..]
    )
}

/// The index of the line that sets `key` inside `section`.
fn find_key_line(lines: &[String], section: &str, key: &str) -> Option<usize> {
    let mut current: Option<&str> = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = section_header(line) {
            current = Some(name);
            continue;
        }
        if current == Some(section)
            && let Some(parsed) = key_line(line)
            && parsed.key == key
        {
            return Some(index);
        }
    }
    None
}

/// Add `key=value` to `section`, creating the section at the end when it is
/// absent.
fn insert_key(lines: &mut Vec<String>, section: &str, key: &str, value: &str) {
    if let Some(header) = lines
        .iter()
        .position(|line| section_header(line) == Some(section))
    {
        // The section's block runs until the next section header; insert before
        // any trailing blank lines so the key joins the section's last value.
        let mut end = lines[header + 1..]
            .iter()
            .position(|line| section_header(line).is_some())
            .map_or(lines.len(), |offset| header + 1 + offset);
        while end > header + 1 && lines[end - 1].is_empty() {
            end -= 1;
        }
        lines.insert(end, format!("{key}={value}"));
        return;
    }
    // A new section goes at the end, after a blank separator line. The trailing
    // empty string `split('\n')` leaves for a final newline is moved to the end
    // so the file keeps exactly one.
    // An empty file is the single empty line `split('\n')` produces; clear it so
    // the appended section is not preceded by a blank.
    if lines.len() == 1 && lines[0].is_empty() {
        lines.clear();
    }
    let ends_with_newline = lines.len() > 1 && lines.last().is_some_and(String::is_empty);
    if ends_with_newline {
        lines.pop();
    }
    if lines.last().is_some_and(|line| !line.is_empty()) {
        lines.push(String::new());
    }
    lines.push(format!("[{section}]"));
    lines.push(format!("{key}={value}"));
    if ends_with_newline {
        lines.push(String::new());
    }
}

/// A failure reading or patching an ini.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IniError {
    /// A section is not in the surface.
    #[error("`{surface}` may not set section `[{section}]`")]
    UnknownSection {
        /// The surface's name.
        surface: String,
        /// The refused section.
        section: String,
    },
    /// A key is not in the surface.
    #[error("`{surface}` may not set `[{section}] {key}`")]
    UnknownKey {
        /// The surface's name.
        surface: String,
        /// The section.
        section: String,
        /// The refused key.
        key: String,
    },
    /// A value is not accepted for a key.
    #[error("[{section}] {key}={value} is not allowed: {reason}")]
    InvalidValue {
        /// The section.
        section: String,
        /// The key.
        key: String,
        /// The refused value.
        value: String,
        /// What the key accepts.
        reason: String,
    },
    /// A `Section.Key=Value` string was malformed.
    #[error("`{text}` is not a setting: {reason}")]
    MalformedSetting {
        /// The text that failed.
        text: String,
        /// Why.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPTS: &str = "\
; OptiScaler.ini — upstream defaults
[Upscalers]
; which upscaler D3D12 games get
Dx12Upscaler=fsr4   ; overridden by the menu
Dx11Upscaler=fsr4

[FrameGen]
Enabled=false
FGType=fsr3
";

    fn optisurface() -> IniSurface {
        IniSurface::optiscaler()
    }

    #[test]
    fn patch_replaces_a_value_and_keeps_comments_and_ordering() {
        let patched = optisurface()
            .patch(OPTS, &[IniSetting::new("FrameGen", "Enabled", "true")])
            .unwrap();
        assert_eq!(
            patched,
            "\
; OptiScaler.ini — upstream defaults
[Upscalers]
; which upscaler D3D12 games get
Dx12Upscaler=fsr4   ; overridden by the menu
Dx11Upscaler=fsr4

[FrameGen]
Enabled=true
FGType=fsr3
"
        );
    }

    #[test]
    fn patch_keeps_an_inline_comment_on_the_replaced_line() {
        let patched = optisurface()
            .patch(
                OPTS,
                &[IniSetting::new("Upscalers", "Dx12Upscaler", "xess")],
            )
            .unwrap();
        assert!(
            patched.contains("Dx12Upscaler=xess   ; overridden by the menu"),
            "{patched}"
        );
    }

    #[test]
    fn crlf_line_endings_survive_a_patch() {
        let text = "[HelixSR]\r\nInt8=off\r\nWaveSize=auto\r\n";
        let patched = IniSurface::helixsr()
            .patch(text, &[IniSetting::new("HelixSR", "Int8", "on")])
            .unwrap();
        assert_eq!(patched, "[HelixSR]\r\nInt8=on\r\nWaveSize=auto\r\n");
    }

    #[test]
    fn patch_adds_a_missing_key_to_an_existing_section() {
        let text = "; a\ndlssnr-amd.ini\n[Network]\nModel=standard\n";
        let patched = IniSurface::dlssnr()
            .patch(text, &[IniSetting::new("Network", "model_scale", "75")])
            .unwrap();
        assert_eq!(
            patched,
            "; a\ndlssnr-amd.ini\n[Network]\nModel=standard\nmodel_scale=75\n"
        );
    }

    #[test]
    fn patch_appends_a_missing_section() {
        let text = "[Network]\nModel=standard\n";
        let patched = IniSurface::dlssnr()
            .patch(text, &[IniSetting::new("NRControl", "MenuKey", "End")])
            .unwrap();
        assert_eq!(
            patched,
            "[Network]\nModel=standard\n\n[NRControl]\nMenuKey=End\n"
        );
    }

    #[test]
    fn patch_refuses_an_unknown_section() {
        let error = optisurface()
            .patch(OPTS, &[IniSetting::new("Secrets", "Token", "x")])
            .unwrap_err();
        assert!(
            matches!(error, IniError::UnknownSection { ref section, .. } if section == "Secrets"),
            "{error}"
        );
    }

    #[test]
    fn patch_refuses_an_unknown_key() {
        let error = optisurface()
            .patch(OPTS, &[IniSetting::new("FrameGen", "SecretToggle", "true")])
            .unwrap_err();
        assert!(
            matches!(error, IniError::UnknownKey { ref key, .. } if key == "SecretToggle"),
            "{error}"
        );
    }

    #[test]
    fn patch_refuses_an_invalid_enum_value() {
        let error = IniSurface::helixsr()
            .patch(
                "[HelixSR]\nInt8=auto\n",
                &[IniSetting::new("HelixSR", "Int8", "maybe")],
            )
            .unwrap_err();
        assert!(
            matches!(&error, IniError::InvalidValue { reason, .. } if reason.contains("auto, on, off")),
            "{error}"
        );
    }

    #[test]
    fn patch_refuses_an_out_of_range_number() {
        for value in ["10", "200", "lots"] {
            let error = IniSurface::dlssnr()
                .patch(
                    "[Network]\nModel=x\n",
                    &[IniSetting::new("Network", "model_scale", value)],
                )
                .unwrap_err();
            assert!(
                matches!(&error, IniError::InvalidValue { reason, .. } if reason.contains("25..=100")),
                "{value}: {error}"
            );
        }
        // The boundary values are accepted.
        assert!(
            IniSurface::dlssnr()
                .patch(
                    "[Network]\nModel=x\n",
                    &[IniSetting::new("Network", "model_scale", "25")],
                )
                .is_ok()
        );
    }

    #[test]
    fn a_refused_patch_leaves_the_input_untouched() {
        // One valid setting and one invalid one: nothing is applied.
        let error = optisurface()
            .patch(
                OPTS,
                &[
                    IniSetting::new("FrameGen", "Enabled", "true"),
                    IniSetting::new("FrameGen", "Enabled", "maybe"),
                ],
            )
            .unwrap_err();
        assert!(matches!(error, IniError::InvalidValue { .. }), "{error}");
        // `patch` is pure, so the original string is unchanged by construction;
        // this asserts the caller's copy too.
        assert!(OPTS.contains("Enabled=false"));
    }

    #[test]
    fn read_returns_an_allowed_value_or_none() {
        let surface = optisurface();
        assert_eq!(
            surface.read(OPTS, "FrameGen", "Enabled").unwrap(),
            Some("false".to_string())
        );
        // An allowed key the file does not set reads as absent.
        assert_eq!(
            IniSurface::helixsr().read(OPTS, "HelixSR", "Int8").unwrap(),
            None
        );
        // An unknown key or section is refused, not read as absent.
        assert!(surface.read(OPTS, "Upscalers", "Dx11Upscaler").is_err());
        assert!(surface.read(OPTS, "Nope", "Dx12Upscaler").is_err());
    }

    #[test]
    fn current_lists_only_allowed_keys() {
        let values = optisurface().current(OPTS);
        let keys: Vec<&str> = values.iter().map(|value| value.key.as_str()).collect();
        assert_eq!(
            keys,
            ["Dx12Upscaler", "Enabled", "FGType"],
            "Dx11Upscaler is not on the surface"
        );
        assert_eq!(values[0].section, "Upscalers");
        assert_eq!(values[0].value, "fsr4");
    }

    #[test]
    fn parses_a_setting_string() {
        let setting = IniSetting::parse("HelixSR.Int8=on").unwrap();
        assert_eq!(setting, IniSetting::new("HelixSR", "Int8", "on"));
        assert!(IniSetting::parse("no-equals").is_err());
        assert!(IniSetting::parse("NoDot=x").is_err());
    }

    #[test]
    fn an_empty_file_gets_a_section_without_a_leading_blank() {
        let patched = IniSurface::helixsr()
            .patch("", &[IniSetting::new("HelixSR", "Int8", "on")])
            .unwrap();
        assert_eq!(patched, "[HelixSR]\nInt8=on");
    }
}
