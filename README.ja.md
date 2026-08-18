<h1 align="center">MineDock</h1>

<p align="center">
  <strong>Minecraft Java Editionのワールドを扱う、Windowsファースト・ローカルファーストなライブラリ。</strong>
</p>

<p align="center">
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/Rust-1.85%2B-000000?logo=rust&logoColor=white">
  <img alt="GPUI 0.2.2" src="https://img.shields.io/badge/GPUI-0.2.2-5A67D8">
  <img alt="Windows 11" src="https://img.shields.io/badge/Platform-Windows%2011-0078D4?logo=windows11&logoColor=white">
  <img alt="開発中" src="https://img.shields.io/badge/Status-Work%20in%20progress-F2C94C">
</p>

<p align="center">
  <a href="README.md">English</a> | 日本語
</p>

MineDockは、Minecraftサーバーのフォルダではなく、長期的に保持される「World」を中心に扱う。サーバープロセスはワールドにサーバープロファイルを通じて紐づく実装上の詳細として扱い、Dockerを必須としない。

## 現在の実装状況

実装計画のPhase 0〜5までが完了しており、アプリケーションシェルとCore/Runtimeの基盤が揃っている。

- GPUI 0.2.2で動作するデスクトップアプリケーション
- メタデータのみのワールド作成ウィザードを備えたダークテーマのWorld Library
- Survival / Hardcore / CreativeのYAMLテンプレートを組み込み、厳密にパース
- スキーマバージョン付きで、アトミックに書き込まれるローカルメタデータ
- 明示的なワールドライフサイクル状態と、起動時の古いActive状態からのリカバリ
- バックグラウンドでのJava検出とバージョン解析
- HTTPSリダイレクト上限、ハッシュ/サイズ検証、バージョン付きキャッシュを備えた公式Vanillaリリース/JARプロバイダー
- 明示的なEULA同意を永続化するゲート、決定論的な`server.properties`生成、`online-mode=true`の起動時検証
- シェルを介さないネイティブJavaプロセスアダプター。gracefulな`stop`、上限付きエスカレーション、ログイベント、ワールド単位の起動予約、Windows Job Objectによるプロセス管理に対応

現在の画面上のワークフローは、メタデータのみのワールドを作成して一覧表示するところまで。Worldカードには永続化されたライフサイクル状態とJavaの準備状況が表示されるが、**Start/Stopは無効化されている**。Phase 6で、公式バージョン解決、EULA確認ダイアログ、プロビジョニング、ライフサイクルコマンドをUIへ接続した後に有効化する予定。

アプリを開いたりワールドを作成したりするだけでは、実際のMinecraft Server JARのダウンロードやサーバー起動は行わない。バックアップとネットワーク関連のUXも未実装。

MVPの最終目標は[docs/MVP.md](docs/MVP.md)に記載しており、チェックリストの進捗は[PLAN.md](PLAN.md)で管理している。

## ソースから実行

Windows 11での前提環境:

- `rustfmt`と`clippy`を含むstable Rust
- Desktop development with C++ワークロードとWindows SDKを含むVisual Studio 2022 Build Tools

リポジトリのルートから実行:

```powershell
cargo run -p minedock-app --locked
```

MineDockはデフォルトでメタデータを`%LOCALAPPDATA%\MineDock`配下に保存する。開発用に分離されたディレクトリを使う場合:

```powershell
$env:MINEDOCK_DATA_DIR = "$PWD\.local-minedock-data"
cargo run -p minedock-app --locked
```

このオーバーライド先にはユーザーデータやランタイム状態が含まれるため、リポジトリへコミットしないこと。

## 検証

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build -p minedock-app --locked
```

テストでは、テンプレート検証、メタデータのラウンドトリップと破損検出、ワールド状態遷移、Java出力解析、provider/EULA/provisionの不変条件、ライフサイクル監視、制御されたネイティブプロセス動作をカバーしている。

実際のMojangからのダウンロードと、本物のMinecraftサーバープロセスの起動は、意図的にオフラインテストスイートの対象外としている。

## リポジトリ構成

```text
MineDock/
├─ crates/
│  ├─ minedock-core/   # ドメイン、永続化、テンプレート、provider、ライフサイクル規則
│  └─ minedock-app/    # GPUIとネイティブ/Windowsアダプター
├─ templates/          # 組み込みの宣言的ワールドテンプレート
├─ docs/               # MVP、UI、セキュリティ、調査メモ、ADR
├─ ARCHITECTURE.md
├─ PLAN.md
└─ SPEC.md
```

`minedock-core`にはGPUIやWindows固有の型を持ち込まない。ネイティブJava検出、HTTPS通信、プロセス生成、Job Object、アプリデータロック、GPUI表示は`minedock-app`側に置く。実装済みの境界については[ARCHITECTURE.md](ARCHITECTURE.md)を参照。

## 安全上の制約

- MineDockはMinecraft EULAへ暗黙的に同意しない。サーバー成果物の取得や`eula.txt`の書き込みを行う前に、ユーザーが明示的に同意した記録を別途永続化する必要がある。
- 生成および起動時に検証されるサーバー設定では`online-mode=true`を維持する。
- テンプレートから実行ファイルのパス、スクリプト、ダウンロードURLを指定することはできない。
- Vanilla providerが受け入れるHTTPS接続先は、許可リストに含まれる公式Mojang/Minecraftのauthorityのみに限定する。
- Minecraft Server JAR、ワールド、Javaランタイム、ログ、バックアップ、シークレット、ローカルアプリデータをコミットしてはならない。
- MVPではDockerを必須とせず、UPnP設定、Mod、Plugin、クラウド機能、アカウントログインも対象外とする。

Hardcoreは特別なアーキテクチャモードではなく、組み込みテンプレートの1つとして扱う。
