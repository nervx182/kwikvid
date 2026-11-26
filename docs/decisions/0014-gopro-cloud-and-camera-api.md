# 0014 GoPro cloud and Open GoPro camera import

Status: accepted (2026-11-26, spike #159). The cloud half is blocked on the
owner (#168).

**This is not legal advice.** It is an engineering reading of public terms,
written so the owner knows what to confirm. Each claim links its source.
Anything not checked against a primary source is marked (guess). Sources
were read on 2026-11-26.

## Context

The owner would like to "pull videos from GoPro cloud". Roadmap I lists
"GoPro cloud (opt-in)" and camera import, and `docs/architecture.md` plans a
`kv-cloud` crate for both. #157 / #158 import from a mounted camera volume or
SD card. Before writing any client we need the API, auth, terms and legal
position.

## Findings

### 1. Is there an official API for GoPro cloud media?

- **Yes, on paper: a "Cloud API (BETA)", by request.** GoPro's developer
  tools page lists "Cloud API (BETA): Programmatic access to our media cloud
  service. Enables upload, download and playback of media", with a
  "Request Access" link to `gopro.com/en/us/connect`
  ([developer tools](https://gopro.com/en/us/info/developer-tools),
  [Open GoPro landing page](https://gopro.com/en/gb/info/open-gopro)).
  Verified that the listing exists.
- **No public documentation, eligibility rules, terms, pricing or auth flow**
  were found. Nothing says whether an open-source desktop app (one client
  id shipped to every user) is eligible, or whether HiLight metadata is
  exposed. A May 2025 question to GoPro asking for exactly this API has no
  answer ([gopro/labs discussion #1400](https://github.com/gopro/labs/discussions/1400)).
  Third parties describe GoPro cloud as having "no documented API for
  third-party developers"
  ([Blober article](https://dev.to/mckabue/gopro-cloud-transfer-bloberio-connects-to-gopro-cloud-3eb6)).
- Who can get access and on what terms: **unknown, can only be learned by
  applying.** That needs the owner's identity and is out of scope for an agent
  (#168, `needs-human`).

### 2. The undocumented web-app API

What community tools do (all read from their READMEs, none run):

| Project | Auth | Licence |
| --- | --- | --- |
| [itsankoff/gopro-plus](https://github.com/itsankoff/gopro-plus) | copy the `gp_access_token` (a JWT) and `gp_user_id` cookies from a logged-in browser | MIT |
| [gopro-media-library-downloader](https://npmjs.com/package/gopro-media-library-downloader) | copy the `Authorization: Bearer ...` header from the web app's `search` request | MIT-style |
| [gkoefner/goproCloudDownloaderExtension](https://github.com/gkoefner/goproCloudDownloaderExtension) | runs inside the logged-in browser session; `https://api.gopro.com/media/search` (100 per page), then `/media/{id}/download` | **GPL-3.0** (excluded, ADR 0005) |
| [dustin/gopro](https://github.com/dustin/gopro) (Haskell) | username + password, token lasts "a day or less", refreshed | BSD-3-Clause |

- **Auth:** the web app's own session: a short-lived bearer JWT, from a
  browser login (cookies) or by replaying the login with the user's password.
  How the web login handles 2FA or captchas was not checked (guess: it does,
  which breaks password replay). No OAuth flow for third parties exists.
- **Stability:** private, unversioned endpoints of a web app; they can change
  without notice (guess, but typical). Several tools exist since about 2020,
  which suggests the `api.gopro.com/media/*` shape has been fairly stable
  (guess).
- **GoPro Terms of Use** ([terms](https://gopro.com/en/sn/legal/terms),
  "Last Updated Date: April 11, 2024"), section 9 "General Prohibitions and
  Acceptable Use Standards", forbids:
  - to "access or search the Service or Content or download Content from the
    Service" through any engine, software or tool (including spiders, robots,
    crawlers, data-mining tools) "other than the software and/or search agents
    provided by GoPro or other generally available third party web browsers";
  - to "decipher, decompile, disassemble or reverse engineer any of the
    software used to provide the Service";
  - to probe, scan or test, or circumvent security or authentication measures.

  Section 5.1 grants a licence "solely for your personal and non-commercial
  purposes". Section 6: GoPro "does not claim any ownership rights in your
  Content". Section 12: GoPro may disable access "at any time, with or
  without cause". Section 16: California law, or for EU residents the law of
  their Member State. Section 20: US residents, mandatory arbitration and
  class-action waiver. The GoPro app licence also forbids reverse engineering
  the app ([app licence](https://gopro.com/en/hr/legal/embedded/mobileapplicense)).

  **Plain reading: a kwikvid client on the undocumented API would breach
  section 9 for every user who runs it**, even though the content is theirs.
  The realistic risks are to the users (account suspension under section 12)
  and to the project (a takedown or contract claim), more than criminal law.

- **Legal position for an open-source client accessing the user's own data**
  (not legal advice):
  - *US, computer-crime law:* after *Van Buren* (2021), breaching terms of
    service alone is generally not "exceeding authorized access" under the
    CFAA, but contract claims survive: in *hiQ v. LinkedIn* the court held the
    user agreement's scraping ban enforceable as a contract, and hiQ settled
    with a $500,000 judgment and an injunction
    ([Morgan Lewis summary, Dec 2022](https://www.morganlewis.com/blogs/sourcingatmorganlewis/2022/12/linkedin-v-hiq-landmark-data-scraping-suit-provides-guidance-to-data-scrapers-and-web-operators)).
    hiQ scraped public data with fake accounts, so the facts differ (users
    fetching their own data with their own login), but the contract route is
    the relevant risk.
  - *EU, GDPR Art. 20 portability*
    ([text](https://www.legislation.gov.uk/eur/2016/679/article/20)): a right
    to receive personal data the user "provided to a controller" in a
    machine-readable format. It is exercised as a *request to GoPro*, and
    GoPro's privacy policy offers exactly that (form on community.gopro.com
    or privacy@gopro.com, "a copy of your information in a structured,
    commonly used and machine-readable format")
    ([privacy policy](https://gopro.com/en/mx/legal/privacy-policy), effective
    2024-11-06). It does not give a third-party client a right to use private
    endpoints against the terms.
  - *EU Data Act* (applies since 2025-09-12): gives users access to data from
    connected products, but "content (e.g. ... audiovisual material) [is] out
    of scope" ([Commission explainer](https://digital-strategy.ec.europa.eu/en/factpages/data-act-explained)).
    Footage is not covered; camera telemetry might be (guess).
  - *EU Software Directive Art. 6* ([text](https://www.legislation.gov.uk/eudr/2009/24/article/6))
    allows decompiling a program for interoperability under strict
    conditions. It concerns code the user lawfully holds, not access to a
    remote service, so it does not help here (guess, ask a lawyer).

  **What the owner would need to confirm** (with a lawyer if the API route is
  ever reconsidered): (a) whether section 9 binds a user who runs a
  third-party client on their own account, in their jurisdiction; (b) whether
  publishing such a client exposes the project to claims; (c) GoPro's answer
  to a Cloud API access request (#168), which would make (a) and (b) moot.

### 3. Security (for any future cloud client)

Applies only if #168 yields an official API; nothing is built now.

- **Off by default, opt-in:** a "Connect GoPro account" action in settings;
  no network call to GoPro before that. Sign-out deletes every stored token.
- **What is stored:** only the OAuth refresh token (and access token if
  needed) and the account id. **Never the password**: sign-in happens in the
  system browser (OAuth 2.0 authorization code with PKCE, RFC 8252, guess
  about what GoPro supports). No cookies copied from browsers.
- **Where:** the macOS Keychain (generic password, service `kwikvid.gopro`),
  via `keyring` (MIT OR Apache-2.0, apple-native backend) or
  `security-framework` (MIT OR Apache-2.0). Other platforms: the OS secret
  store through `keyring`. Never in the project file, the app config JSON,
  the cache or logs.
- **Logging:** tokens, auth headers, signed download URLs and the account
  email are never logged at any level; `tracing` fields carry media ids and
  byte counts only. Token types implement `Debug` as `"<redacted>"`.
- **MCP / CLI:** the MCP server never exposes tokens; a cloud listing tool, if
  any, would be opt-in like the app.

### 4. Alternatives

- **Download from gopro.com or Quik, then import.** Fully supported by the
  terms (a browser is allowed). Subscribers get "unlimited" GoPro footage "at
  100% quality" in the cloud ([subscriptions](https://gopro.com/en/au/shop/subscriptions)),
  so originals, with GPMF and HiLights in the file, can be downloaded (guess
  that the cloud original keeps `udta` boxes byte-for-byte). Whether a
  multi-file download arrives as a zip or many files was not verified (guess);
  in 2020 the web app issued one request per file
  ([blog, 2020](https://medium.com/devops-one-problem-at-a-time/gopro-plus-i-dunno-a95cee94091a)).
  `Session::import` scans one folder non-recursively, so recursive folder and
  zip import makes this one step (#166).
- **GoPro desktop auto-offload.** GoPro Quik for desktop (Mac, Feb 2024) syncs
  projects with the mobile app and subscribers get camera-to-cloud upload
  ([Digital Camera World](https://digitalcameraworld.com/news/gopro-offers-new-premium-subscription-tier-and-releases-quik-desktop-app)).
  No documented folder it offloads to that kwikvid could watch was found
  (guess). Not pursued.
- **GDPR export request** (EU users): legal, slow, manual; mention in user
  docs, nothing to build.
- **Open GoPro for a connected camera** (below): official, no account.

### 5. Open GoPro (camera, not cloud)

- **What:** GoPro's official camera API over BLE, Wi-Fi and wired USB
  ([docs](https://gopro.github.io/OpenGoPro/)). GoPro calls the Camera
  Control API "Free to use and published under open-source license"
  ([developer tools](https://gopro.com/en/us/info/developer-tools)). The
  Python SDK is MIT ("Copyright © 2021-2024 GoPro, Inc.", `LICENSE` in
  `demos/python/sdk_wireless_camera_control/`; PyPI `open-gopro` 0.22.0 MIT).
  The repo's root `LICENSE` file currently holds only a third-party-notices
  paragraph, so "the whole spec is MIT" is not verified. We would implement
  documented HTTP calls, not copy code, so this does not block us.
- **Models:** HERO9 Black, HERO10 Black, HERO11 Black, HERO11 Black Mini,
  HERO12 Black, HERO13 Black ([supported cameras](https://gopro.github.io/OpenGoPro/ble));
  "≥ Hero 9", with firmware requirements ([FAQ](https://gopro.github.io/OpenGoPro/faq)).
  Newer models are not in that table (guess: they may be supported later).
- **Wired USB transport** (from the official SDK source, `gopro_wired.py`):
  the camera is a USB network device (NCM, also listed for USB streaming in
  the FAQ). IP `172.2X.1YZ.51` where X, Y, Z are the last three serial
  digits, base `http://{ip}:8080/` (plain HTTP, no auth), discoverable by mDNS
  `_gopro-web._tcp.local.`. The SDK first calls
  `gopro/camera/control/wired_usb?p=1`.
- **Media endpoints** (SDK `api/http_commands.py`, `models/media_list.py`):
  `GET gopro/media/list` returns `{id, media: [{d: "100GOPRO", fs: [{n, cre,
  mod, glrv?, ls?, ...}]}]}`; `GET videos/DCIM/<dir>/<file>` downloads;
  `gopro/media/info?path=` gives per-file metadata including `hi` (HiLight
  offsets in ms) and `hc` (count); also `gopro/media/thumbnail`,
  `gopro/media/screennail`, `gopro/media/gpmf`, `gopro/media/turbo_transfer`,
  `gopro/camera/keep_alive`. Wi-Fi AP mode uses `http://10.5.5.9:8080`
  ([media list tutorial](https://gopro.github.io/OpenGoPro/tutorials/camera-media-list));
  COHN (camera on home network) adds HTTPS with a camera certificate and
  Basic auth (SDK `cohn_feature.py`).
- **Does a USB-connected HERO mount as a disk on macOS?** Probably **not**:
  modern GoPros use MTP rather than mass storage, so they show in Image
  Capture, not Finder ([MacPaw](https://macpaw.com/how-to/connect-gopro-mac),
  secondary source; GoPro's own support articles did not load). So #157/#158
  reach SD cards in a reader (and any camera that does mount), not cameras on
  a cable (guess until the owner checks, #167). Whether macOS brings up the
  NCM interface without drivers is also unverified (guess; the SDK says it is
  tested on macOS).
- **Terms:** Open GoPro exists for third parties; no account or cloud is
  involved, so none of the section 9 concerns apply. kwikvid should not
  imply GoPro endorsement or use its logos (guess: GoPro's trademark rules
  were not read).

### 6. Licence check (ADR 0005, `deny.toml`)

| Crate | Use | Licence (crates.io, 2026-11-26) |
| --- | --- | --- |
| `reqwest` 0.13.5 | HTTP client; already in `Cargo.lock` via Tauri | MIT OR Apache-2.0 |
| `ureq` 3.5.2 | lighter blocking alternative | MIT OR Apache-2.0 |
| `mdns-sd` 0.22.0 | optional discovery | Apache-2.0 OR MIT |
| `zip` 9.8.2 / `rc-zip` 5.4.1 | zip import (#166) | MIT / Apache-2.0 OR MIT |
| `keyring` 4.2.0, `security-framework` 3.7.0 | tokens, only if #168 succeeds | MIT OR Apache-2.0 |
| `oauth2` 5.0.0 | only if #168 succeeds | MIT OR Apache-2.0 |
| `btleplug` 0.13.5 | BLE, not planned | MIT/Apache-2.0/BSD-3-Clause |

All are on the allow list. Watch the transitive tree: a TLS stack using
`webpki-roots` brings CDLA-Permissive-2.0, which `deny.toml` does not allow;
wired Open GoPro is plain HTTP, so build `reqwest` with
`default-features = false` and no TLS. Excluded: the GPL-3.0 browser
extension above (we neither vendor nor link it).

No experiments were run and nothing was added under `spikes/`.

## Decision

1. **Cloud: blocked on owner confirmation of GoPro Cloud API (BETA) access
   and terms (#168).** If GoPro grants access under terms that allow an
   open-source desktop client, build opt-in cloud import in `kv-cloud` via
   that API with the security rules in section 3. **Do not build** on the
   undocumented web-app API: it breaches Terms of Use section 9 for every
   user and can break without notice. Until then, the supported route is
   "download from gopro.com / Quik, then import the folder or zip" (#166).
2. **Camera: build Open GoPro wired-USB import** in `kv-cloud` (HERO9 and
   later): list, read HiLights, download into the library folder with the
   same job shape as #157, and offer it in #158's dialog. No credentials,
   no account. BLE, Wi-Fi AP and COHN are not planned for Roadmap I.

## Consequences

- `docs/architecture.md`: the `kv-cloud` row now says Open GoPro wired USB
  first, cloud only via the official API.
- Follow-ups (all `status:triage`, Roadmap I):
  - #165 kv-cloud: Open GoPro wired USB client (list, HiLights, download job), M.
  - #166 kv-session: import a folder tree or .zip downloaded from GoPro cloud, M.
  - #167 Owner check: how a USB-connected HERO appears on macOS, S, `needs-human`.
  - #168 Owner: request GoPro Cloud API (beta) access and record its terms, S, `needs-human`.
  - #169 kv-app: offer USB-connected GoPro cameras (Open GoPro) in the camera
    import dialog, M (blocked by #158, #165).
- No user is asked for GoPro credentials in Roadmap I.
- HiLights added in the Quik app (ADR 0006: not in the file) stay out of
  reach; the cloud API may expose them, which #168 should ask about.
