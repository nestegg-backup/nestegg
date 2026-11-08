<script lang="ts">
  import type { Tone } from "../lib/derive";

  let { tone = "neutral" }: { tone?: Tone } = $props();
</script>

<span class="dot {tone === 'neutral' ? '' : tone}" aria-hidden="true"></span>
