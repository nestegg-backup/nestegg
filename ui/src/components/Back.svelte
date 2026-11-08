<script lang="ts">
  import { t } from "../lib/locale";
</script>

<!-- Only visible on a phone, where list and detail are two screens. -->
<a class="back" href="#/">{t.nav.allPcs}</a>

<style>
  .back { display: none; }
  @media (max-width: 760px) {
    .back { display: inline-block; color: var(--muted); text-decoration: none; padding: 8px 0; margin-bottom: 18px; }
  }
</style>
