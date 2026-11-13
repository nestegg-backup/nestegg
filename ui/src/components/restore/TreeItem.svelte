<script lang="ts">
  import TreeItem from "./TreeItem.svelte";
  import { app } from "../../lib/store.svelte";
  import { archiveSource, browseFailureText, restore, type TreeNode } from "../../lib/restore.svelte";
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
  <div class="node" class:dir={folder} style:--depth={depth}>
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
      <p class="state" style:--depth={depth + 1}>{t.restore.dirLoading}</p>
    {:else if node.children}
      {#if node.children.length}
        <ul>
          {#each node.children as child (child.path)}
            <TreeItem node={child} depth={depth + 1} />
          {/each}
        </ul>
      {:else}
        <p class="state" style:--depth={depth + 1}>{t.restore.dirEmpty}</p>
      {/if}
    {/if}
  {:else if node.failed}
    <p class="state bad" style:--depth={depth + 1}>{browseFailureText(node.error, t)}</p>
  {/if}
</li>

<style>
  /* --depth is the nesting level; --indent the step per level (smaller on a phone, see below). */
  li { --indent: 22px; }
  .node {
    display: grid; grid-template-columns: 28px 22px minmax(0, 1fr) auto; align-items: center; gap: 6px;
    padding: 5px 8px 5px calc(var(--depth) * var(--indent)); border-radius: 6px;
  }
  .node:hover { background: var(--box-hover); }
  .twist {
    appearance: none; border: 0; background: none; width: 28px; height: 28px; cursor: pointer; border-radius: 6px;
    display: grid; place-items: center; color: var(--muted);
  }
  .twist svg { width: 10px; height: 10px; transition: transform 0.15s; }
  .twist[aria-expanded="true"] svg { transform: rotate(90deg); }
  input { margin: 0; width: 17px; height: 17px; accent-color: var(--accent); }
  input:disabled + label { color: var(--muted); }
  /* Names wrap (a long file name or the translated "list of programs ..." label is never cut off). */
  label { min-width: 0; overflow-wrap: break-word; cursor: pointer; }
  .dir label { font-weight: 500; }
  .size { font-size: var(--fs-s); color: var(--faint); }
  .state { font-size: var(--fs-s); color: var(--muted); padding: 4px 0 4px calc(var(--depth) * var(--indent) + 56px); }
  .state.bad { color: var(--bad); }
  /* Phone: a deep folder must leave room for the name. The steps get smaller and the size moves below
     the name, so an ordinary file name is not broken in the middle. */
  @media (max-width: 480px) {
    li { --indent: 12px; }
    .node { grid-template-columns: 28px 22px minmax(0, 1fr); row-gap: 0; }
    .size { grid-column: 3; }
    .size:empty { display: none; }
  }
</style>
