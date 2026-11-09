<script lang="ts">
  import TreeItem from "./TreeItem.svelte";
  import { app } from "../../lib/store.svelte";
  import { archiveSource, restore, type TreeNode } from "../../lib/restore.svelte";
  import { isInventoryArchive } from "../../lib/derive";
  import { f, t } from "../../lib/locale";

  let { node, depth }: { node: TreeNode; depth: number } = $props();

  const label = $derived(
    node.type === "archive"
      ? isInventoryArchive(node.archive)
        ? t.restore.inventory
        : (archiveSource(node.archive, app.config.value) ?? node.archive)
      : node.name,
  );
  const folder = $derived(node.type === "archive" || node.type === "directory");
  const inherited = $derived(restore.inherited(node));
  const checked = $derived(inherited || restore.isSelected(node));
  const disabled = $derived(inherited);
  const id = $derived(`n-${node.archive}-${node.path}`.replace(/[^A-Za-z0-9_-]/g, (c) => `_${c.charCodeAt(0).toString(16)}`));
</script>

<li>
  <div class="node" class:dir={folder} style:padding-left="{depth * 22}px">
    {#if folder}
      <button
        class="twist"
        type="button"
        aria-expanded={node.open}
        aria-label={node.open ? t.restore.collapse(label) : t.restore.expand(label)}
        onclick={() => restore.toggle(node)}
      >
        <svg viewBox="0 0 10 10" aria-hidden="true"><path d="M3 1.5 7 5 3 8.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" /></svg>
      </button>
    {:else}
      <span></span>
    {/if}
    <input type="checkbox" {id} {checked} {disabled} onchange={(e) => restore.setSelected(node, e.currentTarget.checked)} />
    <label for={id}>{label}</label>
    <span class="size">{node.size != null && node.type === "file" ? f.size(node.size) : ""}</span>
  </div>
  {#if node.open}
    {#if node.loading}
      <p class="state" style:padding-left="{(depth + 1) * 22 + 56}px">{t.restore.dirLoading}</p>
    {:else if node.children}
      {#if node.children.length}
        <ul>
          {#each node.children as child (child.path)}
            <TreeItem node={child} depth={depth + 1} />
          {/each}
        </ul>
      {:else}
        <p class="state" style:padding-left="{(depth + 1) * 22 + 56}px">{t.restore.dirEmpty}</p>
      {/if}
    {/if}
  {:else if node.failed}
    <p class="state bad" style:padding-left="{(depth + 1) * 22 + 56}px">{t.restore.dirFailed}</p>
  {/if}
</li>

<style>
  .node { display: grid; grid-template-columns: 28px 22px minmax(0, 1fr) auto; align-items: center; gap: 6px; padding: 5px 8px 5px 0; border-radius: 6px; }
  .node:hover { background: var(--box-hover); }
  .twist {
    appearance: none; border: 0; background: none; width: 28px; height: 28px; cursor: pointer; border-radius: 6px;
    display: grid; place-items: center; color: var(--muted);
  }
  .twist svg { width: 10px; height: 10px; transition: transform 0.15s; }
  .twist[aria-expanded="true"] svg { transform: rotate(90deg); }
  input { margin: 0; width: 17px; height: 17px; accent-color: var(--accent); }
  input:disabled + label { color: var(--muted); }
  label { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; cursor: pointer; }
  .dir label { font-weight: 500; }
  .size { font-size: var(--fs-s); color: var(--faint); }
  .state { font-size: var(--fs-s); color: var(--muted); padding: 4px 0; }
  .state.bad { color: var(--bad); }
</style>
