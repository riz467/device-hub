# Logicool PRO X SUPERLIGHT (LIGHTSPEED) 解析メモ

2026-11-26 実機解析。**読み取りのみ** (`crates/devhub-hid/examples/logiprobe.rs`) で取得し、
デバイスへの書き込みは一切していない。生ダンプは
[`crates/devhub-hid/fixtures/logitech/prox-superlight-c547.txt`](../crates/devhub-hid/fixtures/logitech/prox-superlight-c547.txt)。

Logicool は Web configurator が無く、G HUB はネイティブアプリなので `ADD_DEVICE.md` の
「JS バンドルを読む」手順は使えない。代わりにプロトコル (HID++ 2.0) 自体が
**機能 ID の自己申告** を持つので、デバイスに全機能を列挙させて既知の機能 ID の get 系だけを叩いた。

## 1. 識別

| 項目 | 値 |
| --- | --- |
| 受信機 | `046d:c547` "USB Receiver" (LIGHTSPEED)。fw 04.02 build 0009 (reg 0xF1) |
| HID++ コレクション | if=2 `up=ff00 u=01` (short, report 0x10, 7 byte) / `up=ff00 u=02` (long, report 0x11, 20 byte) |
| 通常入力 | if=0 マウス (`0001:02`)、if=1 キーボード/コンシューマ/システム |
| ペア済み | slot #1 のみ (reg 0x02 → `00 01 00`)。無線 PID **0x4093**、受信機上の名前 "PRO X" (reg 0xB5 0x20/0x40) |
| マウス (device index 1) | HID++ **4.2**、DeviceName "PRO X Wireless"、type 3 (mouse) |
| ファーム | `MPM25.01 build 0018` (アプリ)、`BL125.01 build 0018` (ブートローダ)、type 5 (ハード rev?) build 00b6 |

- device index 0xFF (受信機自身) は HID++ 1.0 (Root.ping が 1.0 error 0x01 = invalid sub-id)。
  レジスタは 0x81 (short read) / 0x83 (long read) で読める。
- 2..6 は error 0x08 (unknown device)。マウスは常に index 1。
- **スリープ中のマウスは応答しない** (timeout)。起床すると `11 01 00 1d 04 02 ..` (自分の ping の遅延応答) と
  HID++ 1.0 の接続通知 (0x41) が来る。ドライバは timeout を「切断」ではなく「スリープ」と扱い、
  0x41 通知で再読込するのがよい。

## 2. 機能一覧 (FeatureSet, 27 個 + Root)

公開機能 (type 00) だけが UI 対象。`60` = hidden+engineering は工場用なので **触らない**
(DeviceReset 0x1802、GpioAccess 0x1803、RfTest 0x1890 等)。

| idx | ID | 名前 | 用途 |
| --- | --- | --- | --- |
| 0 | 0000 | Root | getFeature / ping |
| 1 | 0001 | FeatureSet | 列挙 |
| 2 | 0003 | DeviceFwVersion | ファーム表示 |
| 3 | 0005 | DeviceName | 名前 |
| 4 | 1d4b | WirelessDeviceStatus | 再接続通知 (broadcast) |
| 5 | 0020 | ConfigChange | (G HUB が設定変更の cookie に使う) |
| 6 | 1004 | UnifiedBattery | **電池** |
| 8 | 8100 | OnboardProfiles | **DPI ステージ・レート・ボタン割り当て (永続)** |
| 9 | 8110 | MouseButtonSpy | ボタン押下の監視 (ボタンテスト用) |
| 10 | 8060 | AdjustableReportRate | **ポーリングレート** |
| 11 | 2201 | AdjustableDpi | **DPI** |
| 12 | 1500 | ? | 未調査 |
| 7 | 8070 | ColorLedEffects (hidden) | LED は無い機種なので不使用 |

**無いもの**: ReprogControlsV4 (0x1B04) → ボタン割り当ては「オンボードプロファイルの書き換え」でしか変えられない。
ExtendedAdjustableReportRate (0x8061) / ExtendedAdjustableDpi (0x2202) も無い → 最大 1000Hz、X/Y 個別 DPI 無し
(これらは PRO X SUPERLIGHT 2 系の機能)。

## 3. 各機能の読み取り値

| 機能 | 要求 | 応答 | 解釈 |
| --- | --- | --- | --- |
| 1004 fn0 getCapabilities | — | `0f 03` | 対応レベル bitmask 0x0f / flags 0x03 (充電式 + 残量% あり) |
| 1004 fn1 getStatus | — | `52 08 00` | **82%**、level 8 (full)、0 = 放電中 |
| 2201 fn0 getSensorCount | — | `01` | センサー 1 |
| 2201 fn1 getSensorDpiList(0) | `00` | `00 00 64 e0 32 64 00` | 0x0064 = 100、`0xE032` = 「範囲・刻み 50」、0x6400 = 25600 → **100〜25600 / 50 刻み** |
| 2201 fn2 getSensorDpi(0) | `00` | `00 03 20 03 20` | 現在 **800** / 既定 800 |
| 8060 fn0 getReportRateList | — | `8b` | bit n = (n+1) ms → 1/2/4/8 ms = **1000/500/250/125 Hz** |
| 8060 fn1 getReportRate | — | `01` | 現在 1 ms = **1000 Hz** |
| 8110 fn0 | — | `05` | 監視できるボタン 5 個 |
| 8100 fn0 getProfilesDescription | — | `01 04 01 05 01 05 10 00 ff 0a 04` | 下表 |
| 8100 fn2 getMode | — | `01` | **onboard モード** (1=onboard / 2=host) |
| 8100 fn4 getCurrentProfile | — | `00 01` | プロファイル 1 (sector 0x0001) |
| 8100 fn11 getCurrentDpiIndex | — | `00` | ステージ 0 |

OnboardProfiles の記述子: memory model 1 / profile format **4** / macro format 1 / プロファイル 5 /
OOB (ROM) プロファイル 1 / ボタン 5 / sector 16 個 / **sector size 255** / mechanical layout 0x0a / various info 0x04。

## 4. オンボードプロファイルの形式 (profile format 4)

`8100 fn5 memoryRead(sector_hi, sector_lo, off_hi, off_lo)` で 16 byte ずつ読める。
offset は `sector_size - 16` (= 239) まで (240 以上は error 0x02 invalid argument)。

### 4.1 sector 0x0000 = ディレクトリ

4 byte × n: `sector(BE16) enabled(u8) ff`、`ff ff` で終端。

```
00 01 01 ff | 00 02 00 ff | 00 03 00 ff | 00 04 00 ff | 00 05 00 ff | ff ff ...
```

→ プロファイル sector 1〜5、**有効なのは sector 1 だけ** (G HUB が「オンボードメモリ」に書いた 1 個)。

### 4.2 プロファイル sector (255 byte)

| offset | 長さ | 内容 | sector 1 (現用) | sector 2〜5 (工場既定) |
| --- | --- | --- | --- | --- |
| 0x00 | 1 | レポート間隔 ms | 01 (1000Hz) | 01 |
| 0x01 | 1 | 既定 DPI ステージ | 00 | 01 |
| 0x02 | 1 | DPI シフト時ステージ | 00 | 00 |
| 0x03 | 2×5 | DPI ステージ (LE16、0 = 未使用) | 800 / – / – / – / – | 400 / 800 / 1600 / 3200 / 6400 |
| 0x0D | 3 | ステージ色 RGB | ff ff ff | ff ff ff |
| 0x10 | 1 | power mode | ff | ff |
| 0x11 | 1 | angle snapping | 00 | 00 |
| 0x20 | 4×16 | ボタン割り当て | 下記 | 同 |
| 0x60 | 4×16 | G-Shift 時のボタン | 同上 | ff (未使用) |
| 0xA0 | 48 | プロファイル名 UTF-16LE | "PROFILE_NAME_DEFAULT" | ff |
| 0xD0 | 11×2 | LED 効果 ×2 (本機は LED 無し) | `00…1f 40…` | 同 |
| 0xFD | 2 | **CRC-16/CCITT-FALSE (init 0xFFFF, poly 0x1021) を BE で** | 03 6b | 51 9b |

CRC は 6 sector すべてで計算値と一致を確認済み (データ 0x00〜0xFC の 253 byte に対して)。

ボタン 4 byte: `80 01 00 01` = type 0x80 (HID) / sub 0x01 (マウスボタン) / bitmask 0x0001 (ボタン 1)。
現用はボタン 1〜5 がそのまま (`…0001`, `…0002`, `…0004`, `…0008`, `…0010`) = 標準配置。
キー割り当ては sub 0x02 (キーボード: modifier + HID usage)、コンシューマは sub 0x03、
DPI 切替などの特殊機能は type 0x90、マクロは type 0x00 (sector アドレス) — **これは libratbag/Solaar の
公開実装に基づく想定で、この個体では未観測**。

## 5. Device HUB への組み込み方針

| 段階 | 内容 | デバイスへの書き込み |
| --- | --- | --- |
| A. 読み取り対応 | 電池・DPI・レート・現在プロファイル・ファーム表示。8110 でボタンテスト | 無し |
| B. 一時変更 (host モード) | 8100 setMode(2) → 2201 setSensorDpi / 8060 setReportRate | RAM のみ。電源を切ると onboard 設定に戻る。常駐アプリが起動・再接続時に再適用する方式 |
| C. 永続変更 (onboard 書き換え) | プロファイル sector を組み立て → CRC → memoryAddrWrite / memoryWrite / memoryWriteEnd → setCurrentProfile | **フラッシュ書き込み**。G HUB の「オンボードメモリ」保存と同じ |

推奨は A → C。B はソフト常駐が前提になり G HUB を消した意味が薄い。C は G HUB 無しで
どの PC でも設定が効く (現状の 800 DPI / 1000Hz もこの方式で入っている)。

C の安全策:
- 書く前に **全 sector を `%APPDATA%\DeviceHub\backup\logitech\<serial>-<日時>.bin` に退避** (今回のダンプ形式で十分)。
- 書くのは現用プロファイル 1 sector だけ。ディレクトリ (sector 0) と ROM (OOB) には触らない。
- 書いた直後に memoryRead で読み戻し、CRC と内容を照合。失敗時は退避から戻す。
- 1 回の保存で書く sector は 1 個。スライダー操作ごとには書かない (確定ボタンでまとめて書く)。
- 書き込み系の関数番号 (8100 fn1 setMode / fn3 setCurrentProfile / fn6 memoryAddrWrite / fn7 memoryWrite /
  fn8 memoryWriteEnd、2201 fn3 setSensorDpi、8060 fn2 setReportRate) は **公開実装からの想定で未検証**。
  実装時に 1 つずつ確認する。

### コード上の作業

1. `devhub-core/src/model.rs`: `DriverKind::Logitech` を追加。
2. `devhub-core/src/registry.rs`: `vid 0x046D / pid 0xC547 / up 0xFF00 / u 0x02`、kind Mouse、
   transport Wireless24G。無線 PID 0x4093 は受信機の reg 0xB5 から取れるので、受信機 PID ではなく
   **ペア先の無線 PID で機種を確定** する (同じ受信機 PID を他機種も使う)。有線接続時の PID は未確認。
3. `devhub-hid/src/logitech.rs` (新規): HID++ transport (long 0x11 送信、software id で自分の応答だけ拾う、
   HID++1.0/2.0 エラー、0x41/1d4b 通知でスリープ・再接続を扱う)、機能 index のキャッシュ、
   `MouseCaps { dpi_min: 100, dpi_max: 25600, dpi_step: 50, max_stages: 5, xy_split: false, stage_colors: false, buttons: 5 個 }`、
   `MouseState` ⇔ プロファイル sector の相互変換 (+ CRC)、ポーリングレートは 125/250/500/1000。
4. フィクスチャ (`fixtures/logitech/`) で sector のデコード・エンコード・CRC の単体テスト。

### 注意

- G HUB は削除済み。G HUB と同時に動かすとプロファイルを上書きし合うので、併用はしない。
- G HUB が残した設定データ `%LOCALAPPDATA%\LGHUB` は onboard の内容とは独立 (消しても本体設定は残る)。

## 6. 実装状況

### 段階 A (読み取り) — 2026-11-26 実装・実機確認済み

- `devhub-hid/src/logitech.rs` (`DriverKind::Logitech`)、registry に `046d:c547 FF00:02` を追加。
- 機種表は持たず、FeatureSet の自己申告から caps を組み立てる (2201 → DPI 範囲、8060 → レート、
  8100 → ボタン数・ステージ数、1004/1000 → 電池)。表示名は DeviceName、model は `logi-<名前>`。
- オンボードモードでは現用プロファイル sector を読み、CRC が合ったときだけ DPI ステージ・ボタンを表示する。
- 通知: 同じ受信機の short/long コレクションを別ハンドルで開き、0x41 (リンク断/復帰)・1D4B (再接続)・
  1004 (電池)・2201/8060/8100 (本体側の変更) を拾う。開いた時に寝ていても、起きた時点で機能を列挙し直す。
- `apply` は何も送らず `NotImplemented`。ポーリングレートは `enabled_when` で操作不可にしてある。
- 単体テスト 9 件 (fixture の CRC・現用/工場プロファイルの解釈・DPI/レート一覧・ボタン型)。
- 実機 (`examples/logiread.rs` + アプリ画面): 名前 PRO X Wireless / FW MPM 25.01.B0018 / 電池 82% /
  800 DPI 1 段 / 1000Hz / 標準 5 ボタン。スリープ表示、変更操作時の「表示のみ」トーストを確認。

(段階 A 時点の「性能タブのポーリングが薄く表示されない」問題は、段階 C で `enabled_when` を外したので解消。)

### 段階 C (オンボードプロファイル書き込み) — 2026-11-26 実装・実機確認済み

- 対象: ポーリングレート (byte 0 = 間隔 ms)・DPI ステージ 1〜5 個 (byte 3.. LE16、空き枠 0)・
  既定ステージ (byte 1)・ボタン割り当て (0x20.. 4 byte。マウス/キー/メディア/無効、読めない型は生のまま書き戻す)。
- 手順: 現在 sector を読む → CRC 確認 → 該当 byte だけ変更 → CRC 付け直し → 同じなら書かない →
  (初回のみ) 退避 → 8100 fn6 `[sector, 0000, len]` / fn7 ×16 (端数 0xFF 詰め) / fn8 →
  読み戻して一致確認 (不一致なら 1 回再試行) → fn3 `[sector]` で再読込 → 使用中ステージを fn 0xC で復元。
- 現在ステージの切替は fn 0xC のみ (フラッシュに書かない)。
- 書かないもの: ROM sector (>= 0x0100)、host モード時のプロファイル (host モードでは 2201 fn3 / 8060 fn2 の RAM 設定のみ。未実機確認)、
  左クリック (slot 0) の変更、スクロール/ダブルクリック等の 0x80 で表せない割り当て。
- 退避: 最初の書き込み前に sector 0 と全ユーザー sector を
  `%APPDATA%\DeviceHub\backups\logitech\<model>-<UTC日付>-<時刻>.txt` (fixtures と同じ書式) に保存。
  保存できなければ書かない。書き戻しは `cargo run -p devhub-hid --example logirestore -- "<ファイル>"`
  (Device HUB 本体を終了してから)。
- sector の読み書きは `op` ロックで直列化 (fn6〜fn8 の途中に memoryRead を挟まない)。
- 単体テスト 14 件 (編集+CRC、同値なら同一バイト、ボタンの往復、ダンプ書式の往復 等)。
- 実機 (`examples/logie2e.rs`): 500Hz / DPI [400,800,1600] 2 段目 / ステージ切替でフラッシュ不変 /
  戻る→音量+ / 左クリック変更・範囲外 DPI の拒否 / 退避からの書き戻しで sector がバイト一致 — 全 10 項目 OK。
  1 回の書き込みは約 1.5 秒 (初回は退避込み)。
