# PS4 FPKG workflow

Digital Forge's PS4 conversion takes a base-game FPKG and a matching update FPKG supplied by the user. Select the base package first, then the update package. Digital Forge inspects their package metadata and requires confirmed fake signing, a game and patch category, and matching title and content IDs before merging them with its managed OrbisPkgTool backend. The merged game FPKG is validated before delivery.

PS4 DLC packages remain separate. A supplied DLC FPKG is validated and delivered as its own package; a prepared add-on folder can be built as a separate DLC FPKG. DLC is not merged into the game package.

The PS4 workflow does not query Sony for updates, download retail updates, request a console firmware version, or run ShadPKG. A retail package is not a substitute for a fake signed base or update package. Digital Forge does not decrypt retail PS4 packages in this workflow.
