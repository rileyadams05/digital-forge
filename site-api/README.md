# Digital Forge successful-test reporting

The Digital Forge website, reporting Worker, and result records belong to `rileyadams05/digital-forge`. Vortex Prime supplies the parent domain only; its repository and website are not used or modified. The public site is `https://digital-forge.vortex-prime-emu.com/` and the reporting API is `https://digital-forge-api.vortex-prime-emu.com/`.

Visitors sign in with GitHub, enter a game ID and test details, and attach a Digital Forge `.log` or `.txt` file (maximum 5 MiB). The Worker creates a branch and pull request in the Digital Forge repository with the result JSON and game log. A reviewer must approve and merge it before the result appears in the public catalog. **The repository is public, so the log is visible to everyone as soon as the pull request is opened. Submitters must remove personal or device-identifying information first.** Logs are stored only in GitHub; this service uses no separate file or database storage.

Failed tests continue to be submitted as GitHub Issues in the Digital Forge repository. Successful results use pull requests. Neither flow writes to the Vortex Prime repository.

## Hosting setup

### 1. GitHub Pages domain

In `rileyadams05/digital-forge` → Settings → Pages, set the custom domain to:

    digital-forge.vortex-prime-emu.com

In the Cloudflare DNS zone for `vortex-prime-emu.com`, set a DNS-only CNAME:

    Name: digital-forge
    Target: rileyadams05.github.io

Wait for GitHub Pages to confirm the domain and issue HTTPS before considering the site live.

### 2. Register the GitHub App

Create the public GitHub App `Digital Forge Test Results` with homepage `https://digital-forge.vortex-prime-emu.com/`, callback URL `https://digital-forge-api.vortex-prime-emu.com/auth/callback`, expiring user authorization tokens enabled, Device Flow enabled, and webhooks disabled. Grant only:

- Contents: read and write
- Pull requests: read and write
- Metadata: read-only (required by GitHub)

Install it only on `rileyadams05/digital-forge`. Do not install it on any Vortex Prime repository. The app must not bypass repository branch protection or merge its own pull requests.

Set these non-secret values in `wrangler.toml`: `GITHUB_APP_ID`, `GITHUB_APP_CLIENT_ID`, and `GITHUB_INSTALLATION_ID`. Keep the private key, OAuth client secret, and `SESSION_SECRET` out of source control and browser code.

### 3. Deploy the Worker

The Cloudflare Worker is configured for `digital-forge-api.vortex-prime-emu.com` in this directory's `wrangler.toml`. It has no R2, KV, database, or external file-storage binding. Add these Worker secrets with Wrangler's interactive secret prompt:

    npx wrangler secret put GITHUB_APP_CLIENT_SECRET
    npx wrangler secret put GITHUB_APP_PRIVATE_KEY
    npx wrangler secret put SESSION_SECRET
    npx wrangler deploy

Set `SESSION_SECRET` to a randomly generated value of at least 32 bytes. The private key is the PEM private key generated for the GitHub App. The API keeps it server-side, creates short-lived App installation tokens, and never exposes it to visitors.

To deploy Worker updates from the Digital Forge repository, add Actions secrets `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` to that repository. The token should be scoped only to Worker deployment in the Cloudflare account. The `success-api.yml` workflow deploys this Worker; the `pages.yml` workflow deploys the website.

### 4. Connect and deploy the site

In `rileyadams05/digital-forge` → Settings → Secrets and variables → Actions → Variables, set:

    DIGITAL_FORGE_SUCCESS_API = https://digital-forge-api.vortex-prime-emu.com

The Pages workflow fails before production publication if this variable is missing. After deployment, verify the API's `/auth/start` redirects to GitHub and make a controlled test submission before announcing the flow.

Protect `master` so result changes require pull-request review. The Worker checks for existing approved results and open result pull requests for the same game ID, creates one JSON result and one attached log file per request, and never merges it. Pending logs are public while their pull requests are open because the GitHub repository is public.

## Digital Forge desktop-app connection

After a successful conversion, Digital Forge checks the public approved-result catalog and matching GitHub failure issues for the detected game ID and shows the resulting status. `Unknown` is shown when there is no approved result or matching failure report. After the user presses Enter to continue, Digital Forge offers to save a local copy of the processing log and open the website's game-status search, regardless of whether a result or issue was found. Pressing Enter at this second prompt skips saving. Digital Forge does not upload logs, sign in to GitHub, or send report data to this API. Users review the status and any matching issues, then sign in and attach the saved log themselves if they choose to submit a report. The browser-based website flow prepares a pull request in `rileyadams05/digital-forge`; it does not merge it.
