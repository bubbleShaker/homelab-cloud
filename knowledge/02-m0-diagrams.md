# knowledge/02 — M0 アーキテクチャ図（UML / mermaid）

M0（`src/lib.rs`）の構造を図で押さえる。文章版は [`01-m0-qa.md`](./01-m0-qa.md)。
GitHub 上ではこの mermaid はそのまま図として描画される。

---

## 1. 全体像：メモリの index がディスクのログを指す

芯の不変条件は「**index は常にログ上の最新値の位置を指す**」。それを1枚にした図。

```mermaid
flowchart LR
    subgraph MEM["メモリ (揮発)"]
        IDX["index: HashMap<br/>String → ValueLoc{offset, size}"]
    end
    subgraph DISK["ディスク (永続)"]
        LOG["追記ログ 00000.log<br/>[rec][rec][rec] ... 末尾に足すだけ"]
    end
    IDX -- "offset で該当バイトを指す" --> LOG
    LOG -- "open() 時に replay して再構築" --> IDX
```

- 書き込みは**必ずログ末尾に append**（シーケンシャルで速い）。
- 読み取りは index で位置を引いて**1回 seek して読む**だけ。
- 再起動時はログを頭から `replay` して index を作り直す（メモリは揮発するから）。

---

## 2. クラス図：登場人物

```mermaid
classDiagram
    class ObjectStore {
        -File log
        -u64 write_offset
        -HashMap index
        +open(dir) ObjectStore
        +put(bucket, key, value) Result
        +get(bucket, key) Result
        +delete(bucket, key) Result
    }
    class ValueLoc {
        +u64 offset
        +u32 size
    }
    note for ValueLoc "値がログのどこにあるか。offset に飛んで size バイト読めば値。Copy な小さい値。"
    ObjectStore o-- ValueLoc : index の値 (HashMap~String,ValueLoc~)
```

補助の自由関数（クラスではなく関数）:

- `replay_log(log)` — `open()` が使用。ログを頭から走査して `(index, write_offset)` を復元する。
- `compose_key(bucket, key)` — `put/get/delete` が使用。bucket と key を `\0` 区切りで連結する。

---

## 3. ログの1レコードのバイト構造

`put("b", "k", "hi")` が書くレコード（キーは compose_key で `b\0k` = 3バイト）。

| offset | 0 | 1–4 | 5–8 | 9–11 | 12–13 |
|--------|---|-----|-----|------|-------|
| 意味 | flags | key_len | value_len | key | value |
| サイズ | 1B | 4B (LE) | 4B (LE) | 3B | 2B |
| 中身(16進) | `00` | `03 00 00 00` | `02 00 00 00` | `62 00 6B` | `68 69` |
| 中身(意味) | 通常 | 3 | 2 | `b \0 k` | `h i` |

合計14バイト。ポイント:

- ヘッダは `flags + key_len + value_len = 9バイト固定`。
- 値の開始位置 = `レコード開始 + 9 + key_len` = `0 + 9 + 3 = 12`。
- よって index には `"b\0k" → {offset: 12, size: 2}` が入る。

---

## 4. シーケンス図：PUT

```mermaid
sequenceDiagram
    actor C as 呼び出し側
    participant OS as ObjectStore
    participant L as ログファイル
    participant I as index

    C->>OS: put("b", "k", "hi")
    OS->>OS: compose_key → "b\0k"
    OS->>OS: レコード組み立て<br/>[flag|key_len|value_len|key|value]
    OS->>L: seek(write_offset) → write_all(record)
    OS->>I: insert("b\0k", {offset:12, size:2})
    OS->>OS: write_offset += 14
    OS-->>C: Ok(())
```

## 5. シーケンス図：GET

```mermaid
sequenceDiagram
    actor C as 呼び出し側
    participant OS as ObjectStore
    participant I as index
    participant L as ログファイル

    C->>OS: get("b", "k")
    OS->>I: get("b\0k")
    alt キーが存在
        I-->>OS: {offset:12, size:2}
        OS->>L: seek(12) → read_exact(2B)
        L-->>OS: "hi"
        OS-->>C: Some("hi")
    else キーが無い
        I-->>OS: None
        OS-->>C: None
    end
```

---

## 6. フローチャート：replay（再起動時の index 復元 + 破損末尾の打ち切り）

reviewer の 🔴 must で入れた「入力を信用しない」検証がここ。

```mermaid
flowchart TD
    A["open: end = ファイル長, offset = 0"] --> B{"offset + 9 <= end ?<br/>(ヘッダ分の残りがあるか)"}
    B -- いいえ --> Z["打ち切り<br/>write_offset = offset"]
    B -- はい --> C["ヘッダ9B読む<br/>→ flags, key_len, value_len"]
    C --> D{"record_end = offset+9+key_len+value_len<br/><= end ?"}
    D -- "いいえ (破損/巨大len)" --> Z
    D -- はい --> E["key を key_len バイト読む"]
    E --> F{"UTF-8 として妥当?"}
    F -- いいえ --> Z
    F -- はい --> G{"flags == tombstone ?"}
    G -- はい --> H["index.remove(key)"]
    G -- いいえ --> I["index.insert(key → offset, size)"]
    H --> J["value を読み飛ばし<br/>offset = record_end"]
    I --> J
    J --> B
    Z --> Y["復元完了: 破損末尾は次の PUT で上書きされる"]
```

- `record_end > end` の判定が、**巨大 len による OOM** と **範囲外レコードの登録**を同時に防ぐ。
- 打ち切った `offset` が次の書き込み開始位置になるので、ゴミは自然に上書き修復される。
