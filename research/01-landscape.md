# research/01 — 技術ランドスケープ調査

自宅の物理マシン上で「AWS の礎を自作する」ための初期調査。
最初の題材を **自作 S3（オブジェクトストレージ）** に定めた理由と、その古典技術の地図。

## なぜ最初の題材が「自作 S3」なのか

候補（自作 S3 / 自作ロードバランサ / 自作 VPC）を比較した結果、S3 を推す:

| 候補 | 低レイヤ度 | ハード無しで始められるか | 計測しやすさ | 拡張の伸びしろ |
|------|-----------|--------------------------|--------------|----------------|
| **自作S3** | 高（ファイルI/O・追記ログ） | ◎ ローカルで完結 | ◎ PUT/GET が明快 | ◎ 分散/レプリカへ伸ばせる |
| 自作LB | 中〜高（epoll・ソケット） | ○ | ○ | ○ |
| 自作VPC | 高（パケット・ルーティング） | △ 物理前提が強い | △ | ○ |

S3 は **「ハード無しの M0 から始めて、物理・分散へ段階的に伸ばせる」** のが決め手。
m5stack の「データ構造を変えて速くする」体験と構造が一致する（追記ログ + index の設計）。

## 自作 S3 の中核アーキテクチャ（定番設計）

調査で判明した定番パターン。これがそのまま「低レイヤで削る」対象になる:

```
PUT /bucket/key
  └→ オブジェクト本体を「追記ログ(WAL)」の末尾に append
  └→ index(埋め込みDB)に {key, log_file, offset, size} を記録

GET /bucket/key
  └→ index で {log_file, offset, size} を引く
  └→ 該当ログの該当オフセットを読み出して返す

DELETE
  └→ index に tombstone（物理削除は compaction 時にまとめて）
```

- **追記ログ(WAL)**: ランダム書き込みを避け、シーケンシャル append にすることで速い。
  ログが閾値（例 2GiB）に達したら read-only に「クローズ」し、新ログを開く。
- **index**: sqlite などの埋め込みDBで key → 位置 を高速引き。
- **compaction**: tombstone や上書きで無効化された領域を回収する後処理。
  → ここが GC 的な低レイヤ最適化のネタになる。

これは AWS の内部（や MinIO / Ceph などのOSS）が採る考え方と同根。
「普段ブラックボックスの S3」の底が抜ける体験になる。

## 触れることになる古典技術（AWS の礎）

- **HTTP / REST**: S3 API そのもの。まずは最小サブセットを自作パース。
- **追記ログ / WAL**: DB・分散システムの基礎。LSM-tree にも通じる。
- **ファイルI/O**: `write`/`fsync`/`mmap`、zero-copy(`sendfile`)。
- **epoll / 非同期I/O**: 多数コネクションを1プロセスで捌く（M5 で本格化）。
- **レプリケーション / 整合性**: 複数ノード化で quorum・最終的整合性に触れる（M4）。

## 実装言語の比較（M0 で確定させる）

| 言語 | 長所 | 短所 | 低レイヤ体験 |
|------|------|------|--------------|
| **Go** | ネットワーク/並行が書きやすい、Pi でも軽い、学習コスト低 | GC があり「削り」の下限がある | ○（epoll は runtime が隠す） |
| **Rust** | zero-copy・所有権で低レイヤを直に触れる、速い | 学習コスト高、開発が遅い | ◎（m5stack 的な削り込みに最適） |
| C | 究極に低レイヤ | 安全性・生産性が低い | ◎だが茨の道 |

**確定: Rust 一本**（2026-07-21 決定）。
理由: 本プロジェクトの目的は「動くものを早く作る」ではなく **低レイヤを直に触る削り込み体験の再現**。
Go は epoll/GC/メモリ配置を runtime が隠すため、一番触りたい層に手が届かない。
2言語(Go→Rust)の継ぎ目コストも避け、M0 から M5 まで Rust で通す。

## ハード調達の選択肢（M2 用・2026年時点）

- **Raspberry Pi 5 (8GB) + NVMe HAT**: 1台 $100〜150。PCIe で NVMe が使え、
  ストレージ系の題材と相性が良い。3台構成が定番。
- **クラスタボード**: DeskPi Super6C（CM4 ×6）で $189.99 など。
- **ミニPC 併用**: 常時起動の母艦をミニPC、Pi を用途特化ノードにする構成が人気。

初手は **Pi 5 (8GB) ×2 + スイッチ + NVMe** あたりが、コストと学びのバランス良い。
※ M0〜M1 はハード不要なので、調達と並行で進められる。

## 参考にした情報源

- [s3-from-scratch (anthonybudd) — ベアメタルで S3 を自作するガイド](https://github.com/anthonybudd/s3-from-scratch)
- [Designing an S3 object storage system — Iván Ovejero（WAL + sqlite index の設計）](https://ivov.dev/notes/s3-object-storage)
- [Build Your Own S3-Compatible Object Storage with MinIO and Java](https://medium.com/devsecops-ai/build-your-own-s3-compatible-object-storage-with-minio-and-java-2e6b0adc4206)
- [Best Raspberry Pi 5 Homelab Projects 2026](https://sudo-build.com/raspberry-pi-5-homelab-projects-2026/)
- [How to Build a Homelab in 2026 — IdleWatt](https://idlewatt.com/guides/how-to-build-a-homelab-2026)
