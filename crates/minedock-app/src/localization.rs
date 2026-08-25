use crate::network::LanAddressUnavailableReason;
use atomic_write_file::AtomicWriteFile;
use minedock_core::{JavaReadiness, JavaUnavailableReason, WorldStatus};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    English,
    Japanese,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiText {
    LocalLibrary,
    Worlds,
    NewWorld,
    Language,
    English,
    Japanese,
    NamePlaceholder,
    NoWorlds,
    LanConnection,
    EndpointInvalid,
    CheckingLan,
    MultipleLan,
    Copy,
    CreateWorld,
    Name,
    Template,
    Minecraft,
    VanillaCurrentRelease,
    Cancel,
    Create,
    SelectTemplate,
    EulaTitle,
    EulaBody,
    OfficialEula,
    ViewOfficialEula,
    Agree,
    Start,
    Stop,
    ForceStop,
    Preparing,
    Starting,
    Stopped,
    Running,
    Stopping,
    BackingUp,
    Failed,
    RecoverRequired,
    StartupError,
    SettingsError,
    WorldDescription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Start,
    Stop,
    ForceStop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaStatus {
    Checking,
    Detected {
        major: u16,
    },
    Ready {
        major: u16,
        executable: String,
    },
    Unavailable {
        reason: Option<JavaUnavailableReason>,
    },
}

impl JavaStatus {
    pub fn from_readiness(readiness: &JavaReadiness) -> Self {
        if let Some(runtime) = &readiness.runtime {
            return Self::Ready {
                major: runtime.version.major.get(),
                executable: runtime.executable.display().to_string(),
            };
        }
        Self::Unavailable {
            reason: readiness.reasons.first().cloned(),
        }
    }

    pub fn detected_from_readiness(readiness: &JavaReadiness) -> Self {
        if let Some(runtime) = &readiness.runtime {
            return Self::Detected {
                major: runtime.version.major.get(),
            };
        }
        Self::Unavailable {
            reason: readiness.reasons.first().cloned(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppSettings {
    schema_version: u16,
    language: Language,
}

impl Language {
    pub fn text(self, key: UiText) -> &'static str {
        match (self, key) {
            (Self::English, UiText::LocalLibrary) => "Local library",
            (Self::Japanese, UiText::LocalLibrary) => "ローカルライブラリ",
            (Self::English, UiText::Worlds) => "Worlds",
            (Self::Japanese, UiText::Worlds) => "ワールド",
            (Self::English, UiText::NewWorld) => "+ New World",
            (Self::Japanese, UiText::NewWorld) => "+ 新しいワールド",
            (Self::English, UiText::Language) => "Language",
            (Self::Japanese, UiText::Language) => "言語",
            (Self::English, UiText::English) => "English",
            (Self::Japanese, UiText::English) => "English",
            (Self::English, UiText::Japanese) => "日本語",
            (Self::Japanese, UiText::Japanese) => "日本語",
            (Self::English, UiText::NamePlaceholder) => "e.g. Sunday Survival",
            (Self::Japanese, UiText::NamePlaceholder) => "例: サバイバルワールド",
            (Self::English, UiText::NoWorlds) => {
                "No worlds yet. Create one from a built-in template to get started."
            }
            (Self::Japanese, UiText::NoWorlds) => {
                "まだワールドがありません。組み込みテンプレートから作成してください。"
            }
            (Self::English, UiText::LanConnection) => "LAN connection",
            (Self::Japanese, UiText::LanConnection) => "LAN接続",
            (Self::English, UiText::EndpointInvalid) => {
                "Connection endpoint unavailable: configured server-port is invalid."
            }
            (Self::Japanese, UiText::EndpointInvalid) => {
                "接続先を表示できません: server-portの設定が無効です。"
            }
            (Self::English, UiText::CheckingLan) => "Checking for usable LAN IPv4 addresses…",
            (Self::Japanese, UiText::CheckingLan) => "使用可能なLAN IPv4アドレスを確認中…",
            (Self::English, UiText::MultipleLan) => {
                "Multiple LAN addresses found. Choose the interface your friend can reach:"
            }
            (Self::Japanese, UiText::MultipleLan) => {
                "複数のLANアドレスが見つかりました。友だちが接続できるインターフェースを選択してください:"
            }
            (Self::English, UiText::Copy) => "COPY",
            (Self::Japanese, UiText::Copy) => "コピー",
            (Self::English, UiText::CreateWorld) => "Create a World",
            (Self::Japanese, UiText::CreateWorld) => "ワールドを作成",
            (Self::English, UiText::Name) => "Name",
            (Self::Japanese, UiText::Name) => "名前",
            (Self::English, UiText::Template) => "Template",
            (Self::Japanese, UiText::Template) => "テンプレート",
            (Self::English, UiText::Minecraft) => "Minecraft",
            (Self::Japanese, UiText::Minecraft) => "Minecraft",
            (Self::English, UiText::VanillaCurrentRelease) => "Vanilla · current release",
            (Self::Japanese, UiText::VanillaCurrentRelease) => "バニラ · 現行リリース",
            (Self::English, UiText::Cancel) => "Cancel",
            (Self::Japanese, UiText::Cancel) => "キャンセル",
            (Self::English, UiText::Create) => "Create",
            (Self::Japanese, UiText::Create) => "作成",
            (Self::English, UiText::SelectTemplate) => {
                "Select a template to see its safe defaults."
            }
            (Self::Japanese, UiText::SelectTemplate) => {
                "テンプレートを選択すると安全な既定値を表示します。"
            }
            (Self::English, UiText::EulaTitle) => "Minecraft EULA",
            (Self::Japanese, UiText::EulaTitle) => "Minecraft EULA",
            (Self::English, UiText::EulaBody) => {
                "Minecraft server software is subject to the official Minecraft EULA. MineDock needs your explicit agreement before it can download or provision server software."
            }
            (Self::Japanese, UiText::EulaBody) => {
                "Minecraftのサーバーソフトウェアには公式EULAが適用されます。MineDockはサーバーソフトウェアをダウンロードまたは準備する前に、明示的な同意を必要とします。"
            }
            (Self::English, UiText::OfficialEula) => "Official EULA",
            (Self::Japanese, UiText::OfficialEula) => "公式EULA",
            (Self::English, UiText::ViewOfficialEula) => "View official EULA",
            (Self::Japanese, UiText::ViewOfficialEula) => "公式EULAを開く",
            (Self::English, UiText::Agree) => "I Agree",
            (Self::Japanese, UiText::Agree) => "同意する",
            (Self::English, UiText::Start) => "START",
            (Self::Japanese, UiText::Start) => "起動",
            (Self::English, UiText::Stop) => "STOP",
            (Self::Japanese, UiText::Stop) => "停止",
            (Self::English, UiText::ForceStop) => "FORCE STOP",
            (Self::Japanese, UiText::ForceStop) => "強制停止",
            (Self::English, UiText::Preparing) => "PREPARING",
            (Self::Japanese, UiText::Preparing) => "準備中",
            (Self::English, UiText::Starting) => "STARTING",
            (Self::Japanese, UiText::Starting) => "起動中",
            (Self::English, UiText::Stopped) => "Stopped",
            (Self::Japanese, UiText::Stopped) => "停止",
            (Self::English, UiText::Running) => "Running",
            (Self::Japanese, UiText::Running) => "稼働中",
            (Self::English, UiText::Stopping) => "Stopping",
            (Self::Japanese, UiText::Stopping) => "停止中",
            (Self::English, UiText::BackingUp) => "Backing Up",
            (Self::Japanese, UiText::BackingUp) => "バックアップ中",
            (Self::English, UiText::Failed) => "Failed",
            (Self::Japanese, UiText::Failed) => "失敗",
            (Self::English, UiText::RecoverRequired) => "RECOVER REQUIRED",
            (Self::Japanese, UiText::RecoverRequired) => "復旧が必要",
            (Self::English, UiText::StartupError) => "Startup error",
            (Self::Japanese, UiText::StartupError) => "起動エラー",
            (Self::English, UiText::SettingsError) => "Settings error",
            (Self::Japanese, UiText::SettingsError) => "設定エラー",
            (Self::English, UiText::WorldDescription) => {
                "Vanilla release, Java, and server files are prepared on Start"
            }
            (Self::Japanese, UiText::WorldDescription) => {
                "バニラのリリース、Java、サーバーファイルは起動時に準備されます"
            }
        }
    }

    pub fn status(self, status: WorldStatus) -> &'static str {
        self.text(match status {
            WorldStatus::Stopped => UiText::Stopped,
            WorldStatus::Preparing => UiText::Preparing,
            WorldStatus::Starting => UiText::Starting,
            WorldStatus::Running => UiText::Running,
            WorldStatus::Stopping => UiText::Stopping,
            WorldStatus::BackingUp => UiText::BackingUp,
            WorldStatus::Failed => UiText::Failed,
        })
    }

    pub fn action(self, action: UiAction) -> &'static str {
        self.text(match action {
            UiAction::Start => UiText::Start,
            UiAction::Stop => UiText::Stop,
            UiAction::ForceStop => UiText::ForceStop,
        })
    }

    pub fn template_name(self, id: &str, fallback: &str) -> String {
        if self == Self::Japanese {
            return match id {
                "vanilla-survival" => "バニラ・サバイバル".into(),
                "hardcore" => "ハードコア".into(),
                "creative" => "クリエイティブ".into(),
                _ => fallback.to_owned(),
            };
        }
        fallback.to_owned()
    }

    pub fn java_status(self, status: &JavaStatus) -> String {
        match (self, status) {
            (Self::English, JavaStatus::Checking) => "Checking Java readiness…".into(),
            (Self::Japanese, JavaStatus::Checking) => "Javaの状態を確認中…".into(),
            (Self::English, JavaStatus::Detected { major }) => {
                format!("Java detected (major {major}; release check pending)")
            }
            (Self::Japanese, JavaStatus::Detected { major }) => {
                format!("Java {major}を検出（リリース確認待ち）")
            }
            (Self::English, JavaStatus::Ready { major, executable }) => {
                format!("Java {major} ready ({executable})")
            }
            (Self::Japanese, JavaStatus::Ready { major, executable }) => {
                format!("Java {major}が利用可能（{executable}）")
            }
            (Self::English, JavaStatus::Unavailable { reason }) => reason.as_ref().map_or_else(
                || "Java runtime unavailable".into(),
                |reason| reason.safe_diagnostics(),
            ),
            (Self::Japanese, JavaStatus::Unavailable { reason }) => reason.as_ref().map_or_else(
                || "Javaランタイムを利用できません".into(),
                |reason| self.japanese_java_unavailable(reason),
            ),
        }
    }

    fn japanese_java_unavailable(self, reason: &JavaUnavailableReason) -> String {
        debug_assert_eq!(self, Self::Japanese);
        match reason {
            JavaUnavailableReason::NoCandidates => "Java実行ファイルが見つかりません。".into(),
            JavaUnavailableReason::CandidateMissing { path } => {
                format!("Java候補が存在しません: {}", path.display())
            }
            JavaUnavailableReason::CandidateNotRegularFile { path } => {
                format!("Java候補は通常のファイルではありません: {}", path.display())
            }
            JavaUnavailableReason::ProbeFailed { path, detail } => {
                format!("Javaを実行できませんでした: {} — {detail}", path.display())
            }
            JavaUnavailableReason::TimedOut { path } => {
                format!(
                    "Javaのバージョン確認がタイムアウトしました: {}",
                    path.display()
                )
            }
            JavaUnavailableReason::NonZeroExit {
                path,
                code,
                diagnostics,
            } => format!(
                "{} は終了コード {:?}で終了しました: {diagnostics}",
                path.display(),
                code
            ),
            JavaUnavailableReason::MalformedVersion { path, diagnostics } => format!(
                "Javaのバージョンを解析できませんでした: {} — {diagnostics}",
                path.display()
            ),
            JavaUnavailableReason::IncompatibleMajor {
                path,
                actual,
                required,
            } => format!(
                "{} はJava {}ですが、Java {}が必要です",
                path.display(),
                actual,
                required
            ),
        }
    }

    pub fn lan_unavailable(self, reason: &LanAddressUnavailableReason) -> String {
        match (self, reason) {
            (Self::English, LanAddressUnavailableReason::NoUsablePrivateIpv4) => {
                "Connection endpoint unavailable: no usable private LAN IPv4 address was found. Check that a Wi-Fi or Ethernet adapter is connected.".into()
            }
            (Self::Japanese, LanAddressUnavailableReason::NoUsablePrivateIpv4) => {
                "接続先を表示できません: 使用可能なプライベートLAN IPv4アドレスが見つかりません。Wi-FiまたはEthernetアダプターが接続されているか確認してください。".into()
            }
            (Self::English, LanAddressUnavailableReason::AdapterEnumerationFailed(detail)) => {
                format!("Connection endpoint unavailable: could not enumerate network adapters: {detail}")
            }
            (Self::Japanese, LanAddressUnavailableReason::AdapterEnumerationFailed(_)) => {
                "接続先を表示できません: ネットワークアダプターを確認できません。Wi-FiまたはEthernetアダプターが接続されているか確認してください。".into()
            }
        }
    }

    pub fn copy_notice(self, endpoint: &str) -> String {
        if self == Self::Japanese {
            format!("{endpoint} をクリップボードにコピーしました。")
        } else {
            format!("Copied {endpoint} to clipboard.")
        }
    }

    pub fn via(self, interface_name: &str) -> String {
        if self == Self::Japanese {
            format!("接続先: {interface_name}")
        } else {
            format!("via {interface_name}")
        }
    }

    pub fn safe_defaults(self, whitelist: bool, max_players: u16, template_name: &str) -> String {
        let whitelist = whitelist.to_string();
        if self == Self::Japanese {
            format!(
                "安全な既定値: online-mode=true · whitelist={whitelist} · {max_players}人 · {template_name}"
            )
        } else {
            format!(
                "Safe defaults: online-mode=true  ·  whitelist={whitelist}  ·  {max_players} players  ·  {template_name}"
            )
        }
    }

    pub fn vanilla_version(self, version: &str) -> String {
        if self == Self::Japanese {
            format!("バニラ · {version}")
        } else {
            format!("Vanilla · {version}")
        }
    }

    pub fn startup_error(self, error: &str) -> String {
        if self == Self::Japanese {
            format!(
                "{} — {error}。変更操作は無効です。",
                self.text(UiText::StartupError)
            )
        } else {
            format!("Startup error — {error}. Mutating actions are disabled.")
        }
    }

    pub fn settings_error(self, error: &str) -> String {
        if self == Self::Japanese {
            format!("{} — {error}", self.text(UiText::SettingsError))
        } else {
            format!("Settings error — {error}")
        }
    }
}

pub fn load_language(root: &Path) -> Result<Language, String> {
    let path = root.join(SETTINGS_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Language::default());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let settings: AppSettings = serde_json::from_slice(&bytes)
        .map_err(|error| format!("settings file is corrupt JSON: {error}"))?;
    if settings.schema_version != SETTINGS_SCHEMA_VERSION {
        return Err(format!(
            "unsupported settings schema version {}",
            settings.schema_version
        ));
    }
    Ok(settings.language)
}

pub fn save_language(root: &Path, language: Language) -> Result<(), String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("could not create app-data root {}: {error}", root.display()))?;
    let path = root.join(SETTINGS_FILE);
    let bytes = serde_json::to_vec_pretty(&AppSettings {
        schema_version: SETTINGS_SCHEMA_VERSION,
        language,
    })
    .map_err(|error| format!("could not serialize settings: {error}"))?;
    let mut file = AtomicWriteFile::open(&path)
        .map_err(|error| format!("could not open atomic settings writer: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("could not write settings: {error}"))?;
    file.flush()
        .map_err(|error| format!("could not flush settings: {error}"))?;
    file.commit()
        .map_err(|error| format!("could not replace settings: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{JavaStatus, Language, UiText, load_language, save_language};
    use crate::network::LanAddressUnavailableReason;
    use minedock_core::{JavaUnavailableReason, WorldStatus};
    use tempfile::TempDir;

    #[test]
    fn missing_settings_default_to_english() {
        let root = TempDir::new().expect("temporary settings root");
        assert_eq!(
            load_language(root.path()).expect("default language"),
            Language::English
        );
    }

    #[test]
    fn japanese_setting_round_trips() {
        let root = TempDir::new().expect("temporary settings root");
        save_language(root.path(), Language::Japanese).expect("save language");
        assert_eq!(
            load_language(root.path()).expect("load language"),
            Language::Japanese
        );
        assert_eq!(Language::Japanese.text(UiText::Worlds), "ワールド");
        assert_eq!(Language::Japanese.status(WorldStatus::Running), "稼働中");
    }

    #[test]
    fn template_names_have_japanese_labels() {
        assert_eq!(
            Language::Japanese.template_name("hardcore", "Hardcore"),
            "ハードコア"
        );
        assert_eq!(
            Language::English.template_name("hardcore", "Hardcore"),
            "Hardcore"
        );
    }

    #[test]
    fn japanese_common_error_states_are_localized() {
        assert_eq!(
            Language::Japanese.java_status(&JavaStatus::Unavailable {
                reason: Some(JavaUnavailableReason::NoCandidates),
            }),
            "Java実行ファイルが見つかりません。"
        );
        assert!(
            !Language::Japanese
                .lan_unavailable(&LanAddressUnavailableReason::NoUsablePrivateIpv4)
                .starts_with("Connection endpoint unavailable")
        );
    }
}
