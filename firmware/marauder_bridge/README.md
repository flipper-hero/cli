# Marauder Bridge for Flipper Zero

This Flipper app connects the `flipper marauder` CLI and the open-source
FlipperHero iOS app to a Marauder board on the Flipper GPIO header. The bridge
uses USART at 115200 baud (Flipper TX pin 13, RX pin 14), enables OTG power
while it runs, and accepts only its fixed command list. It does not install or
update the firmware on the Marauder board.

## Install the prebuilt app

The [Marauder Bridge v0.1.0 release](https://github.com/flipper-hero/cli/releases/tag/marauder-bridge-v0.1.0)
contains `marauder_bridge-momentum-api87.1.fap`. It was built against
[Momentum Firmware](https://github.com/Next-Flip/Momentum-Firmware) commit
`d3f89dfe2` (Flipper API 87.1) and tested on a Flipper Zero running that
firmware. For other firmware or API versions, build from source below.

Attach the Marauder board to the GPIO header, connect the Flipper to your
computer over USB, and install the current CLI with
`cargo install --git https://github.com/flipper-hero/cli.git flipper-cli`.
The older `flipper 0.1.0` release does not include the `marauder` subcommand.
Then install the FAP on the microSD card:

```sh
flipper put marauder_bridge-momentum-api87.1.fap /ext/apps/GPIO/marauder_bridge.fap
flipper marauder commands
flipper marauder scan --duration 5
flipper marauder list
```

The `commands` and `list` checks do not transmit attack frames. A successful
`commands` response confirms the RPC bridge, UART connection and Marauder CLI.

In the open-source FlipperHero iOS build, connect to the Flipper over Bluetooth
and ask the agent to run `marauder` with action `commands`, `scan_ap` or
`list_ap`. The App Store build deliberately excludes the Marauder tool.

## Build for another Flipper firmware

Use the firmware checkout and SDK matching your Flipper. For Momentum:

```sh
git clone https://github.com/flipper-hero/cli.git
git clone https://github.com/Next-Flip/Momentum-Firmware.git
ln -s "$PWD/cli/firmware/marauder_bridge" "$PWD/Momentum-Firmware/applications_user/marauder_bridge"
cd Momentum-Firmware
./fbt fap_marauder_bridge
```

The output is `build/f7-firmware-C/.extapps/marauder_bridge.fap`. Install it at
`/ext/apps/GPIO/marauder_bridge.fap`, then remove only the temporary
`applications_user/marauder_bridge` symlink from the firmware checkout.

## Operation and limits

Scans and radio tests stop automatically after the requested duration. The
bridge also sends `stopscan` when its RPC session closes. It accepts specific
Marauder commands, never arbitrary serial input. The Rust CLI and iOS app cap
response size and strip terminal control sequences. The iOS agent asks for
approval before every transmitting test, including in YOLO mode. Use
transmitting modes only on systems you are authorized to test.

If `commands` times out, check that the FAP matches your Flipper firmware, the
board is powered, and its UART is connected to pins 13/14. If the board uses a
different UART or baud rate, rebuild the bridge after adjusting its source.
