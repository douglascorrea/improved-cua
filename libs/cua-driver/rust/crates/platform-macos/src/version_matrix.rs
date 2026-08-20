//! macOS version detection and the version×feature matrix for private
//! SkyLight APIs (gap G9, issue #8).
//!
//! The driver's core bet is version-fragile private APIs, so "what
//! resolved on *this* macOS version" is itself a capability. This module
//! is the single source of truth for:
//!
//!   1. the running macOS version ([`macos_version`]), and
//!   2. the registry of private symbols the driver depends on
//!      ([`SKYLIGHT_SYMBOLS`]) — what each gates, the first macOS version
//!      known to export it, and whether the core delivery ladder
//!      requires it.
//!
//! [`collect_matrix`] resolves every registry row against the running OS
//! and is consumed by `tools::health_report` (`private_api_matrix`
//! check) and by `input::skylight`'s resolver, which logs every miss
//! against the registry's expectation instead of falling back silently.

use std::str::FromStr;
use std::sync::OnceLock;

// ── macOS version ────────────────────────────────────────────────────────────

/// Parsed `kern.osproductversion`, e.g. `26.5.2`. Ordering is
/// lexicographic over (major, minor, patch), matching release order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MacOsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl MacOsVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for MacOsVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for MacOsVersion {
    type Err = ();

    /// Accepts "26", "26.5", or "26.5.2" — missing components are 0.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.trim().split('.');
        let parse = |p: Option<&str>| -> Option<u32> {
            match p {
                None => Some(0),
                Some(x) => x.parse::<u32>().ok(),
            }
        };
        let major = parse(parts.next()).ok_or(())?;
        let minor = parse(parts.next()).ok_or(())?;
        let patch = parse(parts.next()).ok_or(())?;
        if parts.next().is_some() {
            return Err(());
        }
        Ok(Self::new(major, minor, patch))
    }
}

/// The running macOS version, resolved once per process via
/// `sysctl kern.osproductversion`.
///
/// On detection failure returns `0.0.0` and logs an error: version-gated
/// features then take their checked-failure path (never a guessed
/// fallback), and the `private_api_matrix` health check reports the
/// rows it could not gate.
pub fn macos_version() -> MacOsVersion {
    static VERSION: OnceLock<MacOsVersion> = OnceLock::new();
    *VERSION.get_or_init(|| match read_os_product_version().and_then(|s| s.parse().ok()) {
        Some(v) => v,
        None => {
            tracing::error!(
                "kern.osproductversion unreadable; reporting macOS 0.0.0 — \
                 version-gated private APIs will use checked-failure paths"
            );
            MacOsVersion::new(0, 0, 0)
        }
    })
}

fn read_os_product_version() -> Option<String> {
    let name = c"kern.osproductversion";
    unsafe {
        let mut size: libc::size_t = 0;
        if libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
            || size == 0
        {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

// ── Symbol registry ──────────────────────────────────────────────────────────

/// How a registry row is probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// `dlsym(RTLD_DEFAULT, name)` after loading SkyLight.
    Symbol,
    /// ObjC capability probe: does `SLSEventAuthenticationMessage`
    /// respond to `messageWithEventRecord:pid:version:`? Class exists
    /// on macOS 14 but the factory selector was added in macOS 15
    /// (#1503), so presence is version-gated, not symbol-gated.
    AuthMessageFactory,
}

/// One row of the registry: a private symbol (or version-gated
/// capability) the driver depends on.
#[derive(Debug, Clone, Copy)]
pub struct SymbolSpec {
    /// dlsym name, or a dotted capability label for selector probes.
    pub name: &'static str,
    /// What the symbol gates, in operator-readable terms.
    pub feature: &'static str,
    /// First macOS `(major, minor)` known to provide the row; `None`
    /// means expected on every supported macOS (13+).
    pub since: Option<(u32, u32)>,
    /// `true` when the core background-delivery ladder depends on the
    /// row. Optional rows gate unwired/experimental features and never
    /// fail the health check — they surface as informational.
    pub required: bool,
    pub probe: Probe,
}

impl SymbolSpec {
    /// Whether the running macOS version is expected to provide this row.
    pub fn expected_on(&self, version: MacOsVersion) -> bool {
        match self.since {
            None => true,
            Some((major, minor)) => (version.major, version.minor) >= (major, minor),
        }
    }
}

/// The private-API registry. Keep in sync with the dlsym lookups in
/// `input::skylight` — a unit test cross-checks the two lists.
pub const SKYLIGHT_SYMBOLS: &[SymbolSpec] = &[
    SymbolSpec {
        name: "SLEventPostToPid",
        feature: "SkyLight per-pid event posting (background input)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLEventSetAuthenticationMessage",
        feature: "attach keyboard auth envelope (Chromium, macOS 14+)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "CGEventSetWindowLocation",
        feature: "window-local event coordinates",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLEventSetIntegerValueField",
        feature: "SkyLight event field stamping (pid routing)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "CGSMainConnectionID",
        feature: "WindowServer main connection id",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLPSSetFrontProcessWithOptions",
        feature: "foreground escalation / NSMenu key activation",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLSGetWindowOwner",
        feature: "window → owning connection resolution",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLSGetConnectionPSN",
        feature: "connection → PSN resolution",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "GetProcessForPID",
        feature: "pid → PSN fallback resolution",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "_SLPSGetFrontProcess",
        feature: "prior-frontmost capture for restore",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLPSPostEventRecordTo",
        feature: "focus-without-raise event records (unwired)",
        since: None,
        required: false,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "objc_getClass",
        feature: "ObjC runtime class lookup",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "object_getClass",
        feature: "ObjC metaclass lookup (capability probes)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "sel_registerName",
        feature: "ObjC runtime selector interning",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "class_respondsToSelector",
        feature: "ObjC capability probing (macOS 15 auth-selector gate)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "objc_msgSend",
        feature: "ObjC message send (auth-message factory call)",
        since: None,
        required: true,
        probe: Probe::Symbol,
    },
    SymbolSpec {
        name: "SLSEventAuthenticationMessage.messageWithEventRecord:pid:version:",
        feature: "keyboard auth envelope factory for Chromium targets (class method)",
        since: Some((15, 0)),
        required: true,
        probe: Probe::AuthMessageFactory,
    },
];

/// Look up a registry row by dlsym name. Used by the resolver to phrase
/// miss logs against the version expectation.
pub fn spec_for(name: &str) -> Option<&'static SymbolSpec> {
    SKYLIGHT_SYMBOLS
        .iter()
        .find(|s| s.probe == Probe::Symbol && s.name == name)
}

// ── Matrix ───────────────────────────────────────────────────────────────────

/// One evaluated row: the registry entry plus its resolution on this OS.
#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub name: &'static str,
    pub feature: &'static str,
    pub resolved: bool,
    pub expected: bool,
    pub required: bool,
    /// First macOS version known to provide the row, when version-gated.
    pub since: Option<(u32, u32)>,
}

/// Resolve every registry row against the running macOS version.
///
/// Resolution goes through `input::skylight`'s cached resolver, so
/// collecting the matrix also warms — and logs — the same symbols the
/// input path uses.
pub fn collect_matrix() -> Vec<SymbolRow> {
    let version = macos_version();
    SKYLIGHT_SYMBOLS
        .iter()
        .map(|spec| SymbolRow {
            name: spec.name,
            feature: spec.feature,
            resolved: match spec.probe {
                Probe::Symbol => crate::input::skylight::symbol_resolved(spec.name),
                Probe::AuthMessageFactory => {
                    crate::input::skylight::auth_message_factory_supported()
                }
            },
            expected: spec.expected_on(version),
            required: spec.required,
            since: spec.since,
        })
        .collect()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parses_dotted_forms() {
        assert_eq!("26.5.2".parse(), Ok(MacOsVersion::new(26, 5, 2)));
        assert_eq!("14.0".parse(), Ok(MacOsVersion::new(14, 0, 0)));
        assert_eq!("15".parse(), Ok(MacOsVersion::new(15, 0, 0)));
        assert_eq!(" 26.5.2 ".parse(), Ok(MacOsVersion::new(26, 5, 2)));
        assert!("".parse::<MacOsVersion>().is_err());
        assert!("x.y.z".parse::<MacOsVersion>().is_err());
        assert!("26.5.2.1".parse::<MacOsVersion>().is_err());
        assert!("-1.0".parse::<MacOsVersion>().is_err());
    }

    #[test]
    fn version_display_round_trips() {
        let v = MacOsVersion::new(26, 5, 2);
        assert_eq!(v.to_string(), "26.5.2");
        assert_eq!(v.to_string().parse(), Ok(v));
    }

    #[test]
    fn host_version_is_detected_and_supported() {
        // The driver only supports macOS 13+; on any real host the
        // sysctl probe must land inside or above that range.
        let v = macos_version();
        assert!(
            v.major >= 13,
            "expected macOS 13+, got {v} — sysctl kern.osproductversion broken?"
        );
    }

    #[test]
    fn since_gates_expectation() {
        let auth = SKYLIGHT_SYMBOLS
            .iter()
            .find(|s| s.probe == Probe::AuthMessageFactory)
            .expect("auth factory row exists");
        assert!(!auth.expected_on(MacOsVersion::new(14, 7, 0)));
        assert!(auth.expected_on(MacOsVersion::new(15, 0, 0)));
        assert!(auth.expected_on(MacOsVersion::new(26, 5, 2)));

        let plain = spec_for("SLEventPostToPid").expect("post row exists");
        assert!(plain.expected_on(MacOsVersion::new(13, 0, 0)));
        assert!(plain.expected_on(MacOsVersion::new(26, 5, 2)));
    }

    #[test]
    fn registry_has_no_duplicate_names() {
        let mut seen = std::collections::HashSet::new();
        for spec in SKYLIGHT_SYMBOLS {
            assert!(seen.insert(spec.name), "duplicate registry row {}", spec.name);
        }
    }

    #[test]
    fn matrix_covers_registry_and_resolves_required_rows_on_this_host() {
        let matrix = collect_matrix();
        assert_eq!(matrix.len(), SKYLIGHT_SYMBOLS.len());
        for row in &matrix {
            assert!(!row.name.is_empty());
            assert!(!row.feature.is_empty());
            if row.expected && row.required {
                assert!(
                    row.resolved,
                    "required symbol {} unresolved on macOS {} — \
                     private API drift on a supported OS",
                    row.name,
                    macos_version()
                );
            }
        }
    }
}
