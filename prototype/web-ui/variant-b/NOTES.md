# Variant B

One calm column reads like a short status report: local backup health, attention items, then a quiet PC list. Only Arbeitsrechner has controls. Other machines show their last PBS report and are managed locally. Technical details use disclosures.

Palette: background #202020, surface #2a2a2a, raised controls #373737; text #ebebeb, secondary #aeaeae; one action/focus accent #b6ddbd. Status colors: error #e7a8a2, warning #dbbc8a, success #abc1af. Input outlines: #7a7a7a. CSS uses OKLCH. Font: Manrope, 14/16/22/32 px, tabular figures.

Clickable: four PC details, run now with three simulated outcomes, snapshot selection, tree/search, archive selection, new-folder restore with summary, read-only configuration, connection check. Restore never overwrites. New PCs are initialized locally with `sudo nestegg init`; no central invitation under the updated architecture.

Omitted: real API calls, transfers, authentication, credential handling and key export controls. No remote actions, ZIP or original-location restore. No logos, decorative icons, KPI tiles, pills, gradients, shadows, nested cards or dot-joined metadata.

Known limits: file browsers contain small sample datasets; folder existence checks are simulated. Remote reports may be stale. Remote status and capacity are planned data beyond the current local API. The 05:00 window end, five-minute wake timeout, seven-day overdue threshold, file sizes and precise snapshot times are illustrative assumptions. Google Fonts requires connectivity.

Verified in Chromium at 1280 and 390 px: all screens and run outcomes, search and folder selection, restore validation, local-only controls, keyboard focus, reduced motion and AA color contrast. Final screenshots and helper scripts stay in `/tmp/ui-variant-b/`.
