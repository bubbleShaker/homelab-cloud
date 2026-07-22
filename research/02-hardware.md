# research/02 — M2 ハードウェア調査（何を買うか）

M2「自宅ネットワーク構築」に入る前に、物理ノードとして **何を・どこで・いくらで・何台** 買うかを確定させるための調査。
判断軸は PLAN.md の「体験の再現条件」= **制約が明確 / 数字で殴れる / 物理に触れている**。

> 価格は 2026年7月時点の実勢。**2026年4月のメモリ価格高騰で Pi 5 の 8GB/16GB は値上げ傾向**にあり、
> 変動が大きい。ここの数字は「発注前に各ショップで再確認する前提のレンジ」として扱う。

## 結論（TL;DR）

- **本命: Raspberry Pi 5 8GB を採用**。理由は「ARM・GPIO・発熱・PD 電源」という**制約と物理が濃い**から。
  このプロジェクトの狙い（m5stack の低レイヤ体験の再現）に最も合う。
- **最小構成: まず 1 台をフル装備（NVMe 付き）で買う**。M3（1ノード実運用）まではこれで完結。
  M4（分散）に進む時点で **同一構成をもう 1 台**追加して 2 台にする。
- **⚠ 最重要の発見: SD カード起動は却下、NVMe SSD 必須**。
  本プロジェクトの本丸（M5 = ディスクスループットの削り込み）は、SD カードだと
  I/O がボトルネック＆不安定で**計測が無意味**になる。Pi 5 の PCIe に M.2 HAT で NVMe を載せる。
- **スイッチは M4 まで不要**。M3 の 1 ノードはルータの空きポートに挿すだけ。2 台目を足す時に安ギガスイッチを買う。

## Pi 5 か ミニPC か（判断の核）

| 観点 | Raspberry Pi 5 8GB | ミニPC（Intel N100） |
|------|--------------------|----------------------|
| 制約の濃さ | ◎ ARM・4コア・RAM/帯域が明確に有限。GPIO/発熱/PD で物理を意識 | △ x86 で開発機と同質。余力があり「富豪的に殴れて」しまう |
| 物理に触れる感 | ◎ 基板・クーラー・HAT を自分で組む | ○ 完成品を開けるだけ |
| 素の性能/RAM/ディスク | △ NVMe は HAT 増設が要る。RAM 8GB | ◎ 16GB + NVMe 500GB が標準同梱、2.5GbE も多い |
| ネットワーク | 1GbE 内蔵 | 1〜2.5GbE ×1〜2（分散の帯域計測で有利） |
| 学びの新規性 | ◎ ARM/クロスの発見が多い | △ 普段の x86 と地続きで発見が少ない |
| フル装備の総額(1台) | 約 **25,000〜28,000円** | 約 **25,000〜33,000円** |
| 即戦力 | △ 電源/クーラー/NVMe を別途 | ◎ 箱から出して即 OS |

**要するに**: 総額はほぼ拮抗する。差は「性能/即戦力を取る（ミニPC）」か「制約と物理の学び密度を取る（Pi 5）」か。
本プロジェクトは**わざと制約下で削る**のが目的なので、余力が仇になるミニPCより **Pi 5 が趣旨に合う**。

## 推奨構成 A：Raspberry Pi 5（本命） — 1 台分の BOM

| 品目 | 型番/仕様 | 概算 | 必須度 |
|------|-----------|------|--------|
| 本体 | Raspberry Pi 5 / 8GB | 約 15,290円〜（4月以降変動） | 必須 |
| 電源 | Raspberry Pi 公式 AC アダプター 27W USB-C PD | 約 2,000円 | 必須（PD 不足だと NVMe 給電で不安定） |
| 冷却 | Raspberry Pi 5 公式アクティブクーラー | 約 1,500円 | 必須（無いとサーマルスロットリングで計測がブレる） |
| ストレージ | M.2 HAT（PCIe→NVMe 変換, Waveshare 等） + NVMe SSD 256〜512GB | HAT 約 2,000円 + SSD 約 4,000〜6,000円 | **必須**（計測の生命線） |
| ブート用 | microSD 32GB（初期セットアップ/緊急用。最終的に NVMe ブートへ） | 約 1,000円 | 推奨 |
| ケース | HAT 対応ケース or スタック用 | 約 1,500円 | 任意 |
| **1台合計** | | **約 25,000〜28,000円** | |

- **購入先**: [スイッチサイエンス](https://www.switch-science.com/collections/raspberry-pi-5) / [KSY](https://raspberry-pi.ksyic.com/) が国内正規代理店。本体・純正電源・純正クーラー・HAT が一式そろう。秋月電子でも純正クーラー等を扱う。
  - 本体 8GB: <https://www.switch-science.com/products/9250>
  - 公式 27W 電源: <https://www.switch-science.com/products/10259>
  - M.2 NVMe HAT（Waveshare）: <https://www.switch-science.com/products/10586>
  - コンプリートキット（電源/ケース/SD 込み・割高だが手間ゼロ）: <https://www.switch-science.com/products/9793>
- **迷ったら**: 1 台目だけコンプリートキット + 別途 M.2 HAT & NVMe、で確実に立ち上げる手もある。

## 推奨構成 B：ミニPC（対抗馬） — 参考

- **候補**: Beelink Mini S12 Pro（N100 / 16GB DDR4 / 500GB NVMe）または Beelink EQ13（N200 / **デュアル 2.5GbE**、分散のネットワーク計測に有利）。
- **概算**: S12 Pro 約 25,000〜28,000円、EQ13 約 33,000円前後。RAM・SSD・電源・ケース込みの完成品。
- **購入先**: Amazon.co.jp（Beelink 公式ストア）。※型番で検索して国内在庫を確認する。
  - S12 Pro 参考（国内検索）: <https://www.amazon.co.jp/s?k=Beelink+Mini+S12+Pro+N100+16GB>
  - スペック参照（米 Amazon の型番ページ）: <https://www.amazon.com/dp/B0BVFS94J5>
- **採らない理由**: 総額が Pi 5 とほぼ同じなのに「制約」が薄く、x86 で開発機と地続き＝**学びの新規性が低い**。
  ただし「M4 の分散で 2.5GbE の帯域を数字で見たい」を優先するなら EQ13 は合理的。将来 1 台だけ混ぜる選択肢として保留。

## 最小構成と買い増し計画（何台から始めるか）

```
今:  Pi 5 ×1（フル装備・NVMe）      → M3「1ノード実運用」まで完結。スイッチ不要（ルータ直結）
次:  同一 Pi 5 をもう ×1 + ギガスイッチ → M4「分散・レプリケーション」。ここで初めて 2 台構成
```

- **まず 1 台**。理由: M3（単ノードで自作S3を載せ、M1 ベンチを LAN 越しに回す）までは 1 台で足りる。
  1 台で組み立て・OS・NVMe ブート・疎通の「型」を確立してから 2 台目を複製する方が失敗コストが低い。
- **2 台目は M4 着手時**。同一構成にして「同じ土俵で分散」を計測する。
- **スイッチ**: 2 台目投入時に **アンマネージド ギガスイッチ**（NETGEAR GS305 / TP-Link 等、約 1,500〜2,500円）を追加。
  M3 の 1 ノードはルータ空きポートで足りるので前倒し購入は不要。

### 初期発注リスト（今ポチるもの）

- Raspberry Pi 5 8GB ×1
- 公式 27W USB-C 電源 ×1
- 公式アクティブクーラー ×1
- M.2 NVMe HAT ×1 + NVMe SSD 256〜512GB ×1
- microSD 32GB ×1（初期セットアップ用）
- （任意）HAT 対応ケース ×1

→ **概算 25,000〜28,000円 / 1 台**で M2〜M3 を走り切れる。

## 未決事項（発注前に決める）

- [ ] NVMe 容量: 256GB で足りるか（追記ログ + compaction 検証なら 256GB で十分、余裕なら 512GB）
- [ ] 本体 RAM: 8GB で確定か（in-memory index 前提なら 8GB で妥当。16GB は値上げ幅を見て判断）
- [ ] M4 で 2 台目を Pi 5 同一構成にするか、あえて EQ13（2.5GbE）を混ぜて異種ノードにするか
- [ ] microSD ブート → NVMe ブート移行の手順（次の research で OS/セットアップとして詰める）

## Sources

- [Raspberry Pi 5 8GB — スイッチサイエンス](https://www.switch-science.com/products/9250)
- [Raspberry Pi 5 コレクション — スイッチサイエンス](https://www.switch-science.com/collections/raspberry-pi-5)
- [Raspberry Pi 5 コンプリートキット — スイッチサイエンス](https://www.switch-science.com/products/9793)
- [Raspberry Pi 公式 27W USB-C 電源 — スイッチサイエンス](https://www.switch-science.com/products/10259)
- [Waveshare M.2 NVMe HAT — スイッチサイエンス](https://www.switch-science.com/products/10586)
- [Raspberry Pi Shop by KSY](https://raspberry-pi.ksyic.com/news/page/nwp.id/130)
- [Raspberry Pi 5 用公式アクティブクーラー — 秋月電子](https://akizukidenshi.com/catalog/g/g129327/)
- [N100 ミニPC をサーバーにする — もっちゃん note](https://note.com/mocchan/n/n17c750114f4e)
- [Beelink Mini S12 Pro N100 16GB/500GB — Amazon](https://www.amazon.com/dp/B0BVFS94J5)
