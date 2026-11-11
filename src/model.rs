use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    #[default]
    Apps,
    Homebrew,
    Runtimes,
    Packages,
    Tools,
    Docker,
    System,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::Apps,
        Category::Homebrew,
        Category::Runtimes,
        Category::Packages,
        Category::Tools,
        Category::Docker,
        Category::System,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Apps => "Applications",
            Category::Homebrew => "Homebrew",
            Category::Runtimes => "Runtimes",
            Category::Packages => "Packages",
            Category::Tools => "CLI tools",
            Category::Docker => "Docker",
            Category::System => "macOS",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        let s = s.to_ascii_lowercase();
        Category::ALL.into_iter().find(|c| {
            let k = format!("{c:?}").to_ascii_lowercase();
            k == s || (s == "macos" && *c == Category::System) || (s == "brew" && *c == Category::Homebrew)
        })
    }
}

/// A piece of installed software (inventory).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub category: Category,
    pub provider: String,
    /// Who installed / manages it: "Homebrew cask", "Mac App Store", "Sparkle", "manual"...
    pub source: String,
    pub path: Option<String>,
    pub note: Option<String>,
    /// Groups duplicates (bundle id for apps).
    pub key: Option<String>,
}

/// Can Upkeep carry out the update itself?
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Support {
    /// Run through the official package manager.
    Supported,
    /// Available, but the user must do it (see `hint`).
    #[default]
    Manual,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Update {
    pub name: String,
    pub provider: String,
    pub category: Category,
    pub current: String,
    pub latest: String,
    pub source: String,
    pub support: Support,
    /// argv the executor runs (supported updates only).
    pub command: Option<Vec<String>>,
    /// How to do it by hand, or extra context.
    pub hint: Option<String>,
    pub requires_admin: bool,
    pub requires_restart: bool,
    pub download_size: Option<String>,
    /// Names (same provider) this package depends on.
    pub dependencies: Vec<String>,
    /// Names (same provider) of installed packages depending on this one.
    pub dependents: Vec<String>,
    /// Known problems: deprecated, pinned, incompatibilities.
    pub notes: Vec<String>,
    /// argv restoring `current`, only when the package manager supports it.
    pub rollback: Option<Vec<String>>,
    /// Update of the package manager itself (runs before its packages).
    pub is_self_update: bool,
    /// Language runtime: version jumps can break projects.
    pub is_runtime: bool,
    /// Homebrew cask token, when known: lets the engine drop duplicates between providers.
    #[serde(default)]
    pub cask: Option<String>,
    /// App bundle a Homebrew cask command (re)installs: quarantine is stripped from it when enabled.
    #[serde(default)]
    pub app_path: Option<String>,
}

impl Update {
    pub fn id(&self) -> String {
        format!("{}:{}", self.provider, self.name)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    Warning,
    Info,
    Healthy,
}

impl Severity {
    pub const ALL: [Severity; 4] = [Severity::Critical, Severity::Warning, Severity::Info, Severity::Healthy];
    pub fn title(self) -> &'static str {
        match self {
            Severity::Critical => "Critical",
            Severity::Warning => "Warning",
            Severity::Info => "Info",
            Severity::Healthy => "Healthy",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HealthIssue {
    pub severity: Severity,
    pub title: String,
    pub component: String,
    pub reason: String,
    pub consequence: String,
    pub fix: String,
}

impl HealthIssue {
    pub fn new(severity: Severity, title: impl Into<String>, component: impl Into<String>, reason: impl Into<String>) -> Self {
        HealthIssue {
            severity,
            title: title.into(),
            component: component.into(),
            reason: reason.into(),
            consequence: String::new(),
            fix: String::new(),
        }
    }
    pub fn consequence(mut self, s: impl Into<String>) -> Self {
        self.consequence = s.into();
        self
    }
    pub fn fix(mut self, s: impl Into<String>) -> Self {
        self.fix = s.into();
        self
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ScanResult {
    pub installed: Vec<Installed>,
    pub updates: Vec<Update>,
    /// Non-fatal problems while scanning: shown, never hidden.
    pub warnings: Vec<String>,
    /// Provider-specific findings for System Health.
    pub issues: Vec<HealthIssue>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ProviderStatus {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub enabled: bool,
    pub duration: f64,
    pub installed: usize,
    pub updates: usize,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Snapshot {
    /// Unix seconds; 0 = never scanned.
    pub date: u64,
    pub installed: Vec<Installed>,
    pub updates: Vec<Update>,
    pub warnings: Vec<String>,
    pub issues: Vec<HealthIssue>,
    pub providers: Vec<ProviderStatus>,
    /// Updates hidden by the user's exclusion list.
    pub excluded: usize,
}

impl Snapshot {
    pub fn unverifiable(&self) -> usize {
        self.installed.iter().filter(|i| i.note.as_deref().is_some_and(|n| n.contains("not verifiable"))).count()
    }
}
