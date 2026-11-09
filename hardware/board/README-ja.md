# アダプタ基板

アダプタ専用の基板です。
ESP32-S3-MINI-1-N4R2 モジュールを使います。
このモジュールは技適（201-230385）を取得していて、動作確認に使った YD-ESP32-S3 ボードには技適がありません。

*この設計は、まだ製造も動作確認もしていません。以下の動作の説明は回路図から言えることで、実機で確かめたことではありません。*

[English](README.md)

## 概要

- 35.1 × 26.7 mm、4 層、両面実装、68 部品で、すべて JLCPCB で実装します。
  実装は Standard になります（モジュールと TUSB320 が JLCPCB で「Standard のみ」の部品で、両面に部品があるためです）。
- 両端に同じ USB-C ポートが 1 つずつあります。
  どちらを PC 側につないでもよく、もう一方がターゲットに給電して通信します。
  ポートごとの TUSB320LAI がケーブルの向きと役割を判定して I2C で知らせ、LM66200 が 5 V の来ているポートを選び、TS3USB221 がモジュールの USB をもう一方のポートへ切り替えます。
  LM66200 の状態出力が切替の初期値を決めるので、ファームウェアが動く前から正しい向きにつながります。
  ターゲットへの 5 V は、ポートごとの AP22653（490 mA の電流制限）を通ります。
  ファームウェアは、そのポートの役割判定 IC がシンクの接続を検出している間だけ有効にします。
- ターゲットの UART 用に、2.54 mm の L 字ピンヘッダ（G、TX、RX、V）が表面にあり、ピンは長辺から出ます。
  TX と RX は 2 つの SN74LVC1T45 を通り、そのターゲット側の電源は VREF なので、1.8 V でも 3.3 V でも使えます。
  直列の 330 Ω で、TX 同士を誤接続しても壊れません。
  VREF はヘッダの V ピンから取るか、ヘッダの隣のスライドスイッチを `3V3` にして基板の 3.3 V を使います（3 本だけでつなぐターゲット向け）。
  モジュールは IO1 で VREF の電圧を測れます。
- 両ポートに USB の静電気保護（USBLC6-2SC6）、BOOT ボタン、赤 LED、モジュールの UART0、EN、5 V、3.3 V、GND のテストパッドがあります。
- モジュールのアンテナは、幅 5 mm の銅箔のない帯の上に載ります。
  どの層にも銅箔はなく、銅箔はアンテナ領域の 0.3 mm 下から始まります。
  Espressif はアンテナを基板の端から出す配置を推奨しているので、この配置での電波の飛びは実測が必要です。

部品と JLCPCB の部品番号は `design.py` にあります。
モジュールのピン割り当てもそこにあり、各信号の相手の部品がモジュールのどの辺にあるかで選んでいます。

## ファイル

| ファイル | 内容 |
|---|---|
| `design.py` | 設計の唯一の記述。すべての部品のフットプリント、JLCPCB の部品番号、全ピンのネット。 |
| `layout.py` | 全部品の位置（mm）。 |
| `gen_sch.py` | `design.py` から `board.kicad_sch`、`local.kicad_sym`、`sym-lib-table` を書き出す。 |
| `gen_pcb.py` | `design.py` と `layout.py` から `board.kicad_pcb` を書き出す。外形、フットプリント、設計ルール、グランドプレーン。配線は含まない。 |
| `route.py` | `export` は Freerouting 用の Specctra ファイルを書き出す。`import` は `gen_pcb.py` で基板を作り直し、配線結果を載せ、余裕のある所で 3.3 V の線を太くし、全層にグランドを敷いてビアでつなぐ。 |
| `routing.ses` | Freerouting の結果。配線をやり直すと違う結果になるので保存してある。 |
| `fab.py` | JLCPCB 用の `fab/`（`gerbers.zip`、`bom.csv`、`cpl.csv`、`placement-check.txt`）を書き出す。KiCad の検査を通らない基板、JLCPCB のフットプリントがパッドに載らない部品があれば拒否する。 |
| `jlc_align.py`、`jlc_transforms.json` | 全部品番号について JLCPCB のフットプリントを取得して KiCad のものと照合した結果。`fab.py` が書く回転と原点の元。スクリプトはネットワークが要り、結果は保存してある。 |
| `local.pretty/`、`local.kicad_sym` | スライドスイッチのフットプリント（メーカーの図面から作成）と、KiCad のライブラリにない記号。 |
| `board.kicad_pcb`、`board.kicad_sch`、`board.kicad_pro` | 結果。KiCad 10 で開ける。 |
| `sexpr.py` | 各スクリプトが共用する小さな S 式の読み書き。 |

## 作り直し方

KiCad 10 と、その Python モジュール（`import pcbnew`）、KiCad の記号とフットプリントのライブラリ、Freerouting（2.5.0 で確認）を動かす Java が要ります。

```bash
python3 gen_sch.py && kicad-cli sch erc -o erc.rpt board.kicad_sch     # エラー 0、警告 0
python3 route.py import routing.ses                                    # board.kicad_pcb
kicad-cli pcb drc --severity-all -o drc.rpt board.kicad_pcb            # 違反 0、未接続 0
python3 fab.py                                                         # fab/
```

部品や配置を変えて配線をやり直すときは、次のとおりです。

```bash
python3 gen_pcb.py
python3 route.py export work/board.dsn
java -jar freerouting.jar -de work/board.dsn -do routing.ses -mp 60 -mt 1
python3 route.py import routing.ses
```

Freerouting は毎回は完走しません。
この基板では半分ほどの試行で完走し、そのうちいくつかはグランドの切れ端が未接続のまま残りました。
`BOARD_LENGTH=35.4 python3 gen_pcb.py` のように基板の長さを変えると部品の位置が変わるので、別の試行を手軽に作れます。
`kicad-cli pcb drc` で確かめて、通ったセッションファイルを残してください。

## ファームウェア

この基板向けのファームウェアは、専用の変種と機能フラグでビルドします（既定のビルドは YD-ESP32-S3 開発ボード向けです）。

```bash
cd firmware
ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/adapter.defaults" cargo build --release --features board-adapter
tools/mkimage.sh adapter release        # dist/adapter/
```

この基板での動作（`firmware/src/board.rs`、`uart_link.rs`）は次のとおりです。
どちらのポートから 5 V が来ているか（PC 側）を読み、モジュールの USB をもう一方のポートへ切り替え、2 つの役割判定 IC の役割を I2C で固定し（PC 側はシンク、ターゲット側はソース）、ターゲット側のポートにシンクがつながっている間だけ 5 V を出します。
設定モードでは、コンソールのある PC 側のポートに USB をつないだままにし、5 V は出しません。
UART ヘッダは、設定した TCP ポート番号に 8 を足したポートで、設定したボーレートでブリッジします。
8 は、USB デバイスが持ちうるインタフェース番号の上です。
`status` には、どのポートから給電されているか、各ポートの役割判定、5 V スイッチの異常出力、PC 側のポートが 500 mA しか広告していないかどうか（モジュールとターゲットの合計がそれを超えうる）、VREF の電圧が出ます。

## JLCPCB への発注

`fab/gerbers.zip` をアップロードし、4 層、1.6 mm、ENIG（0.4 mm ピッチの部品があるため）、両面の Standard 実装を選び、`fab/bom.csv` と `fab/cpl.csv` をアップロードします。

JLCPCB は部品を自社のフットプリントで配置し、その原点と 0 度の向きは部品ごとに KiCad と違います（同じ SOT-23-6 でも部品によって向きが違います）。
そのため `cpl.csv` は JLCPCB のフットプリントから計算しています。
`jlc_align.py` が全部品番号のフットプリントを取得し（EasyEDA の公開 API、結果は `jlc_transforms.json`）、パッドを KiCad のものと照合します。
`fab.py` はそれをもとに部品ごとに JLCPCB の原点と回転を書き、さらに「書いたとおりに置いた JLCPCB のパッドが KiCad のパッドに重なるか」を検算します（`fab/placement-check.txt`、部品ごとの最大のずれ。最大 0.36 mm で、これはスライドスイッチの JLCPCB の図とメーカーの図の差です）。
裏面の部品の角度は、裏から見た回転です（JLCPCB はフットプリントを回してから鏡像にします）。
2026 年 11 月 9 日の 2 回の配置プレビューで確かめました。2 回目で、JLCPCB のフットプリントが KiCad から 90 度回っている 3 部品によって決まりました。
LED は、JLCPCB の記号でカソードにあたるピン 2 が KiCad のカソードのパッドに来るように対応づけています。
それでも支払いの前には配置のプレビューを見てください。JLCPCB が何をするかを見られる唯一の場所です。

費用の目安は、2026 年 11 月の JLCPCB の価格表からの概算で、見積もりではありません。
5 枚で約 160 米ドルです（両面の Standard 実装、部品代は 1 枚あたり約 10 米ドル、十数種類の Extended 部品の装填手数料、ヘッダの手はんだ）。

## 設計ルール

JLCPCB が公開している 4 層基板の製造能力の範囲内です。
配線幅 0.15 mm（パッドに入る所でルータが細くした箇所は 0.09 mm で、JLCPCB の下限は 0.09 mm）、間隔 0.127 mm、ビアは穴 0.3 mm、径 0.5 mm、銅箔から基板の端まで 0.25 mm（JLCPCB の要求は 0.2 mm）です。
電源の線は 0.25 mm です。

## 確かめていないこと

- 基板はまだ作っていません。
  回路、ファームウェアからの制御、アンテナを基板の上に載せたときの電波の飛びは未確認です。
- モジュールを組み込んだ製品での技適の表示のしかた。
- スライドスイッチの位置。
  メーカーの図面から、つまみを `3V3` の印刷のほうへ倒すと 3.3 V になるはずです。
  どちらでもファームウェアは IO1 で結果を読めます。
- `cpl.csv` は JLCPCB のフットプリントに対して数値で検算しましたが、この版のファイルで作った配置プレビューはまだ見ていません（発注の節を参照）。
- 3.3 V の 22 µF 6.3 V のコンデンサは、その電圧では定格の半分ほどの容量です。
  モジュールの推奨値は 22 µF そのものを前提にしているので、電圧低下によるリセットが出るなら 1 つ足します。
- USB の信号線は自動配線です。
  ポート B の 2 本は長さが約 18 mm 違い、層も変わります。
  フルスピード（12 Mb/s）では問題になりませんが、高速向けの配線ではありません。
