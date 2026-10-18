# Xbox Store startup crash investigation — 2026-09-03

The installed crashing executable was read back before replacement: 8,429,568 bytes,
SHA-256 `03399e96c22f2a561a4851b4d93c0d8761e7d5ad85aad852f6109ed9d06823b4`.
No notification helper file existed beside that executable. The old connection-status
file predates this investigation and is not evidence that the crashing run connected.

## Source-level fault

The newly added `StoreScreen::Artwork()` called
`ATG::DebugDraw::DrawScreenSpaceTexturedRect`. In the installed SDK's
`Source/Samples/Common/AtgDebugDraw.cpp:610`, that immediately calls
`SimpleShaders::SetDeclPosTex()`. `AtgSimpleShaders.cpp:182` dereferences
`g_pSimpleShadersDevice`; line 18 initializes it to NULL. The Store never called
`SimpleShaders::Initialize` or `BindToNewDevice`. Therefore any successfully loaded
cover texture reached a null-device dereference on its first draw. The effect
pointer was also uninitialized. This happens before artwork staging, helper
extraction, notification startup, or menu rendering.

This is a concrete defect in the added preview, consistent with the observed crash
and absence of the helper file. No exception dump from the original failure was
available, so an original exception instruction address was not recovered.

## Fix and hardware results

- Removed the optional preview and its unused DebugDraw/SimpleShaders dependencies.
  Retained FRONT, BACK, COVER, ICON, FONT, title metadata, and helper resources.
- Extracted each resource from the rebuilt XEX using SDK imagexex and compared
  SHA-256 with its source. Both original PNGs match byte for byte.
- Added flushed startup milestones and resource/loader return codes.
- Hardware logs exposed a second, non-crashing defect: system-namespace artwork
  aliases mounted successfully but none were visible to the title's file calls.
  Using the title's `\\??\\` namespace yields two drives, one Aurora install,
  and successful cover/icon staging on the same console.
- Corrected builds reach the first menu frame and verify the worker connection;
  a fresh Xbox screen capture confirms the Store/Exit menu.
- The notification helper load returns `0xC0000022` (access denied). That failure
  is handled without crashing. Background notification delivery is not verified or
  advertised as working on this console; the menu offers the reopen-to-check fallback.

Final tested XEX SHA-256:
`9dcec6dfa32f05b93b5c7ce360c7a7b779165b9db2552c9ec693862e799858a8`.
Startup log: artwork staging succeeds, helper failure is handled, first frame renders.

The local SDK sources above establish the rendering defect. Additional primary
research checked the native export ordinals and module APIs against
[Xenia's kernel export table](https://github.com/xenia-project/xenia/blob/master/src/xenia/kernel/xboxkrnl/xboxkrnl_table.inc),
[module implementation](https://github.com/xenia-project/xenia/blob/master/src/xenia/kernel/xboxkrnl/xboxkrnl_modules.cc),
and [symbolic-link handling](https://github.com/xenia-project/xenia/blob/master/src/xenia/kernel/xboxkrnl/xboxkrnl_ob.cc).
Actual console return codes, rather than emulator behavior alone, establish the
artwork-mount and helper-load results above.
