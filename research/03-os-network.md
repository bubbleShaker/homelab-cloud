# research/03 — M2 OS・ネットワーク設計

research/02 で確定したハード（**Raspberry Pi 5 8GB / NVMe ブート / まず1台 → M4で2台**）の上に、
**どの OS を・どう無人セットアップし・どんな IP/名前解決で疎通させるか** を設計する。
狙いは M3 で「M0 の自作S3を実機1台に載せ、M1 ベンチを **LAN 越し**に安定して回す」土台を作ること。

> スコープ境界: このレイヤ（OS 導入・IP 採番・SSH・名前解決）は「一度決めたら信頼してよい土台」。
> 削り込みの対象は上の自作S3側（M5）なので、ここは**枯れた定番構成で固める**方針。

## 設計方針（TL;DR）

| 項目 | 採用 | 理由（代替案との比較） |
|------|------|------------------------|
| OS | **Raspberry Pi OS Lite (64-bit)** | 公式・軽量・NVMe ブート実績。Ubuntu Server も可だが、Pi 5 の周辺サポートが厚い純正を採る。GUI 無しで**計測ノイズ最小** |
| 初期導入 | **Raspberry Pi Imager の詳細設定で焼き込み** | hostname/SSH鍵/ロケールを事前設定 → **モニタ・キーボード不要**のヘッドレス起動 |
| ブート媒体 | **NVMe ブート**（SDは緊急用に残す） | SD は I/O ボトルネックで計測が無意味（research/02 の結論）。NVMe から起動 |
| IP 採番 | **ルータの DHCP 予約（MAC固定割当）** | ノード側 config を汚さず一元管理。各ノード固定(nmcli)は台数が増えると管理が分散する |
| 名前解決 | **mDNS(`.local`) + `/etc/hosts` 併記** | mDNS はゼロ設定で `hlc-node1.local` が引ける。ベンチ時のブレ回避に hosts で固定名も持つ二段構え |
| プロビジョニング | **bash スクリプト（手動寄り）** | 2〜3台に Ansible は過剰。手順を体で覚える方が趣旨に合う。台数が増えたら Ansible 化を検討 |

## 1. OS 選定と焼き込み

- **OS**: Raspberry Pi OS Lite **64-bit**（Bookworm 系）。Lite = デスクトップ無し。
  - 64-bit 必須の理由: 8GB RAM をフルに使う / Rust の ARM64 バイナリ / 近年の標準。
  - Rust: `rustup` で `aarch64-unknown-linux-gnu` がそのまま動く。M0/M1 の依存ゼロ実装はクロス無しで実機ビルドできる。
- **焼き込み手順（Raspberry Pi Imager）**:
  1. Imager で OS = Raspberry Pi OS Lite (64-bit) を選択。
  2. **⚙ 詳細設定（歯車）** で以下を事前投入 → これがヘッドレス化の肝:
     - hostname: `hlc-node1`
     - **SSH を有効化 + 公開鍵認証**（パスワードログインは無効に）
     - ユーザー名/初期パスワード、Wi-Fi（有線運用なら不要）、ロケール/タイムゾーン(`Asia/Tokyo`)
  3. まず microSD に焼いて起動確認 → 後述の手順で NVMe へ移行。

### ヘッドレス起動の確認

- 有線 LAN に挿して電源投入 → ルータの DHCP クライアント一覧か `ping hlc-node1.local` で発見。
- `ssh user@hlc-node1.local` で鍵ログイン。モニタは一切不要。

## 2. NVMe ブートへの移行

Pi 5 は PCIe(M.2 HAT) 上の NVMe から直接ブートできる。手順の骨子:

1. microSD で起動した状態で NVMe を HAT に装着（認識確認: `lsblk` に `nvme0n1` が出る）。
2. `rpi-clone` もしくは Imager でシステムを NVMe へコピー（クリーンに焼き直してもよい）。
3. **ブートローダのブート順を NVMe 優先に**設定（`raspi-config` の Advanced → Boot Order、または `rpi-eeprom-config` で `BOOT_ORDER` を NVMe 先頭に）。
4. SD を抜いて NVMe 単独起動を確認。**SD は緊急復旧用に温存**。

- 注意: NVMe への安定給電に **公式27W PD 電源が前提**（research/02）。非力な電源だと PCIe 給電で不安定になる。
- 計測観点: NVMe ブートにすることで、M1 ベンチのディスクスループットが「SD の頭打ち」でなく**SSD 実性能**を反映するようになる。

## 3. IP 設計（DHCP 予約）

家庭用ルータの「DHCP 予約 / 固定IP割当」で、各ノードの **MAC アドレスに固定 IP** を紐づける。

例（サブネットは自宅環境に合わせる。ここでは `192.168.10.0/24` を仮定）:

| ホスト名 | 役割 | IP（予約） | 備考 |
|----------|------|-----------|------|
| （ルータ） | GW/DNS | 192.168.10.1 | 既存 |
| `hlc-node1` | S3ノード#1（M3 単ノード） | 192.168.10.11 | まずこの1台 |
| `hlc-node2` | S3ノード#2（M4 分散で追加） | 192.168.10.12 | 2台目 |
| （ベンチ実行元） | M1 ベンチのクライアント | 192.168.10.20 目安 | 開発機 or もう1ノード |

- **なぜ DHCP 予約か**: ノード側は「DHCP のまま」でよく、OS の network config を書き換えない → 再フラッシュしても IP が変わらず、管理がルータに集約される。
- 代替（各ノード固定）: Bookworm は NetworkManager 管理なので `nmcli con mod` で静的化できるが、台数分の設定が各ノードに分散する。**2〜3台なら DHCP 予約が明快**。

## 4. 名前解決（mDNS + hosts 併記）

- **mDNS(avahi)**: Pi OS は標準で avahi が動き、`hlc-node1.local` が**設定ゼロ**で引ける。少数ノードの疎通・SSH はこれで足りる。
- **`/etc/hosts` 併記**: ベンチ実行時は mDNS 解決のわずかな揺らぎを避けるため、各機に固定エントリを置く:
  ```
  192.168.10.11  hlc-node1
  192.168.10.12  hlc-node2
  ```
  → ベンチのターゲット URL は `.local` でなく素のホスト名/IP を使い、**計測に名前解決コストを混ぜない**。
- 将来（ノード増）: ルータ or 1ノードに dnsmasq を立てて内部ゾーン（`*.hlc.lan` 等）を集中管理する案。今は過剰なので保留。

## 5. 疎通確認チェックリスト（M2 の完了条件）

- [ ] `ssh hlc-node1.local` に**公開鍵で**ログインできる（パスワード無効を確認）
- [ ] `hlc-node1` が **NVMe から起動**している（`findmnt /` が `/dev/nvme0n1p*`）
- [ ] ルータ DHCP 予約で **IP が固定**されている（再起動後も同一 IP）
- [ ] 開発機 → `hlc-node1` へ `ping` / 任意ポートへ `nc` 疎通
- [ ] `/etc/hosts` に固定名エントリ（ベンチ用）
- [ ] （M4準備）2台目 `hlc-node2` を同一手順で複製し、node間 `ping` 疎通

## 6. プロビジョニングの流儀

- 当面は **1枚の bash セットアップスクリプト**（`rustup` 導入・タイムゾーン・`/etc/hosts`・ファイアウォール最小設定・自作S3の配置）を用意し、手順を明示化する。
- Ansible/cloud-init は「2〜3台では過剰」。**ノードが増えて手作業が苦になった時点**で導入する（そのときが導入の“痛み駆動”の合図）。

## M2 → M3 の受け渡し

この設計で M2 の「物理の入口」が通ったら、M3 では:
1. `hlc-node1` に Rust ツールチェーンを入れ、M0 の自作S3をビルド・常駐。
2. 自作S3を **HTTP 化**（PLAN.md 未決事項）— LAN 越し `PUT`/`GET` の口を作る。
3. M1 ベンチを**開発機から LAN 越しに**回し、ローカル計測との差（ネットワーク往復・NVMe 実I/O）を数字で取り直す。

## 未決事項（実機着荷前に詰めておける）

- [ ] 自宅サブネット/GW の実値確認（`192.168.10.0/24` は仮。ルータ管理画面で確認）
- [ ] 自作S3の HTTP 化を M3 のどのタイミングで入れるか（PLAN.md と重複する未決）
- [ ] ベンチのクライアントを「開発機」にするか「2台目ノード」にするか（帯域/CPU の切り分け方針）
- [ ] ファイアウォール（ufw/nftables）の最小ポリシー（S3ポートと SSH のみ開放）

## Sources

- [Raspberry Pi OS ドキュメント（Imager/ヘッドレス設定）](https://www.raspberrypi.com/documentation/computers/getting-started.html)
- [Raspberry Pi 5 NVMe ブート / bootloader 設定](https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#raspberry-pi-boot-eeprom)
- [Raspberry Pi OS ネットワーク設定（NetworkManager/Bookworm）](https://www.raspberrypi.com/documentation/computers/configuration.html#networking)
