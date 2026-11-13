# XIAO キャリア基板

Seeed Studio XIAO ESP32S3（または ESP32C3）を載せる小さな基板です。
XIAO の USB-C コネクタと反対側の端に、相手ボードの電圧で動く UART の 4 ピン L 型ヘッダを出します。

**まだ製造していません。** 実機では何も確認していません。

| | |
|---|---|
| 大きさ | 20.0 × 31.7 mm、2 層 |
| ヘッダ | G、TX、RX、V（2.54 mm、L 型）。並びは `../board` の自作基板と同じです |
| V | 相手ボードの電源電圧（1.65〜5.5 V）。レベル変換 IC の相手側の電源にだけ使います |
| XIAO のピン | D4 = TX、D5 = RX、D1 = V の半分（ADC 用） |
| レベル変換 | SN74LVC1T45 を 2 個。TX と RX に 330 Ω を直列に入れています |
| はんだジャンパ | 裏面にあり、出荷時は開放です。つなぐと V が XIAO の 3.3 V になり、G、TX、RX の 3 線だけでつなぐ 3.3 V の相手に使えます |

XIAO のランドには穴があります。
XIAO を端面の端子で基板に直接はんだ付けすることも、ピンヘッダで立てることもできます。
XIAO の裏面には露出したパッドがあるので、XIAO の下になる表面にはランド以外の銅箔を置いていません。

はんだジャンパをつないだ状態で、V を相手ボードに配線してはいけません。
2 つの電源がつながってしまいます。

## ファイル

`design.py` が唯一の元データ（部品、JLCPCB の部品番号、ネット）で、`layout.py` が部品の位置を決めます。
自作基板と共通のスクリプトは `../board` にあります。

```bash
python3 ../board/gen_sch.py .            # board.kicad_sch
python3 gen_pcb.py                       # board.kicad_pcb（フットプリントとルール）
python3 route.py export work/board.dsn
java -Djava.awt.headless=true -jar freerouting.jar --gui.enabled=false \
    -de work/board.dsn -do routing.ses -mp 20 -mt 1
python3 route.py import routing.ses      # 配線、ビア、グランドのベタ
kicad-cli pcb drc --severity-all -o drc.rpt board.kicad_pcb
python3 ../board/fab.py .                # fab/ に gerbers.zip、bom.csv、cpl.csv
```

配線はやり直すたびに結果が変わるので、`routing.ses` を残しています。
JLCPCB が実装するのは表面だけで、XIAO は手ではんだ付けします。
