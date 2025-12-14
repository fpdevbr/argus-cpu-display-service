<p align="center">
  <img src="https://gamemax-br.com/img/produtos/FOTO%20PARA%20SITE-Photoroom(2).png" alt="GAMEMAX Sigma 520 n2 CPU Cooler" width="600">
</p>

# CPU Cooler Temperature Display via Argus Monitor

When I saw a CPU Cooler with a temperature display for sale I thought: Cool! They've managed to put a thermal probe into the cooler.

Silly me! These cheap Chinese CPU Coolers with seven segment displays depend on USB and crappy proprietary software to display temperature.

Given that I already run Argus Monitor on my computer I took advantage of its data API to update the temperature on the CPU Cooler's display, all while being faster and lighter than the original software.

This script is for the GAMEMAX Sigma 520 n2, but you can change VENDOR_ID and PRODUCT_ID to match your USB display, it also uses CPU0 temp as for AMD they are all the same with Argus Monitor.

## Building

You can compile into an .exe with PyInstaller with the following command:

```bash
pip install -r requirements.txt
pyinstaller --onefile --noconsole --icon=exhaust-fan.ico argus-cpu-display-service.py
