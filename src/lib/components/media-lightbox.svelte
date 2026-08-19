<script lang="ts">
	import { ChevronLeft, ChevronRight, X } from '@lucide/svelte';

	import { Button } from '$lib/components/ui/button';

	export type MediaPreview = {
		id: string;
		path: string;
		kind: string;
		title: string;
		src: string;
	};

	let {
		items,
		index,
		onclose,
		onindex,
		onopen
	}: {
		items: MediaPreview[];
		index: number;
		onclose: () => void;
		onindex: (index: number) => void;
		onopen?: (path: string) => void;
	} = $props();

	const current = $derived(items[index] ?? null);
	const hasPrev = $derived(index > 0);
	const hasNext = $derived(index < items.length - 1);

	function prev() {
		if (hasPrev) onindex(index - 1);
	}

	function next() {
		if (hasNext) onindex(index + 1);
	}

	function onKey(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			onclose();
		} else if (event.key === 'ArrowLeft') {
			event.preventDefault();
			prev();
		} else if (event.key === 'ArrowRight') {
			event.preventDefault();
			next();
		}
	}

	function onBackdrop(event: MouseEvent) {
		if (event.target === event.currentTarget) onclose();
	}
</script>

<svelte:window onkeydown={onKey} />

<!-- svelte-ignore a11y_no_static_element_interactions -->
<!-- svelte-ignore a11y_click_events_have_key_events -->
<div
	class="fixed inset-0 z-50 flex flex-col bg-black/85 text-white"
	onclick={onBackdrop}
	role="dialog"
	aria-modal="true"
	tabindex="-1"
	aria-label={current?.title ?? '预览'}
>
	<div class="flex items-center justify-between gap-2 px-3 py-2">
		<p class="min-w-0 truncate text-sm">{current?.title ?? ''}</p>
		<div class="flex shrink-0 items-center gap-1">
			{#if current && onopen}
				<Button
					variant="ghost"
					size="xs"
					class="text-white hover:bg-white/10 hover:text-white"
					onclick={() => current && onopen(current.path)}
				>
					打开文件
				</Button>
			{/if}
			<Button
				variant="ghost"
				size="icon-xs"
				class="text-white hover:bg-white/10 hover:text-white"
				onclick={onclose}
				aria-label="关闭预览"
			>
				<X class="size-4" />
			</Button>
		</div>
	</div>
	<div class="relative flex min-h-0 flex-1 items-center justify-center px-12 pb-6">
		{#if hasPrev}
			<Button
				variant="ghost"
				size="icon"
				class="absolute left-2 text-white hover:bg-white/10 hover:text-white"
				onclick={prev}
				aria-label="上一项"
			>
				<ChevronLeft class="size-6" />
			</Button>
		{/if}
		{#if current}
			{#if current.kind === 'photo'}
				<img
					src={current.src}
					alt=""
					class="max-h-full max-w-full object-contain"
				/>
			{:else if current.kind === 'video'}
				<!-- svelte-ignore a11y_media_has_caption -->
				<video
					src={current.src}
					class="max-h-full max-w-full"
					controls
					autoplay
					playsinline
				></video>
			{:else if current.kind === 'audio'}
				<audio src={current.src} class="w-full max-w-lg" controls autoplay></audio>
			{/if}
		{/if}
		{#if hasNext}
			<Button
				variant="ghost"
				size="icon"
				class="absolute right-2 text-white hover:bg-white/10 hover:text-white"
				onclick={next}
				aria-label="下一项"
			>
				<ChevronRight class="size-6" />
			</Button>
		{/if}
	</div>
	{#if items.length > 1}
		<p class="text-center text-xs text-white/70 pb-3">{index + 1} / {items.length}</p>
	{/if}
</div>
