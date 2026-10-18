# PS5 Itemzflow workflow

Digital Forge provides a **Receive PS5 game dump** action in its main menu. It listens on the local address and port you choose, receives the one-way TCP stream into a new `.tar` file, and sends that file through Digital Forge's existing archive inspection and import path. The transfer destination must not already exist. Progress is shown in bytes because the sender does not provide a total length.

The console-side dump and transfer service must already be running. Start the receiver in Digital Forge and configure the PS5 to send the data to the PC's local network address and selected port. Choose a local interface address or use `0.0.0.0` to listen on all interfaces. The sample port is editable. Press **Alt+C** to stop the receiver.

The native Itemzflow workflow also accepts an already prepared extracted game folder. Digital Forge checks the executable path, `sce_sys/param.json`, `sce_sys/param.sfo`, matching title IDs, and game-module directory, then copies the folder to the selected destination. It does not extract encrypted PS5 PKGs, prepare/fake-sign executables, register or mount the game on the PS5, or confirm that a game launches.

The supplied Sleirs Dumper file is a console-side payload and is not a Windows executable. NetCat GUI is a separate payload sender; it is not required by Digital Forge's PC receiver and is not bundled. The makefself archive has no included license or source attribution notice, so it is not bundled or run.

Use only game data your PS5 environment is authorised to access. Folder validation and a completed transfer do not prove console compatibility.
