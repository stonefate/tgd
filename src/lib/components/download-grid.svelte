<script lang="ts">
	import { createVirtualizer } from '@tanstack/svelte-virtual';
	import { FileText, Music, Pause, Play, Video } from '@lucide/svelte';
	import { untrack } from 'svelte';

	import type { DownloadItem } from '$lib/bindings';
	import { Badge } from '$lib/components/ui/badge';

	const CELL_MIN = 160;
	const GRID_GAP = 8;
	const CAPTION = 76;

	let {
		items,
		fileSrc,
		kindLabel,
		chatLabel,
		formatSize,
		playingId,
		onpreview,
		onopen,
		onplay
	}: {
		items: DownloadItem[];
		fileSrc: (path: string) => string | null;
		kindLabel: (kind: string | null | undefined) => string;
		chatLabel: (item: DownloadItem) => string;
		formatSize: (raw: string | null | undefined) => string;
		playingId: string | null;
		onpreview: (item: DownloadItem) => void;
		onopen: (path: string) => void;
		onplay: (item: DownloadItem) => void;
	} = $props();

	let scrollEl = $state<HTMLDivElement | null>(null);
	let gridWidth = $state(0);

	const columns = $derived(
		Math.max(1, Math.floor((gridWidth + GRID_GAP) / (CELL_MIN + GRID_GAP)))
	);
	const rowCount = $derived(Math.ceil(items.length / columns));
	const estimateRow = $derived.by(() => {
		if (gridWidth <= 0) return CELL_MIN + CAPTION;
		const cell = (gridWidth - GRID_GAP * (columns - 1)) / columns;
		return Math.round(cell + CAPTION);
	});

	const virtualizer = createVirtualizer({
		count: 0,
		getScrollElement: () => scrollEl,
		estimateSize: () => CELL_MIN + CAPTION,
		gap: GRID_GAP,
		overscan: 4
	});

	$effect(() => {
		const el = scrollEl;
		if (!el) return;
		const ro = new ResizeObserver(() => {
			gridWidth = el.clientWidth;
		});
		ro.observe(el);
		gridWidth = el.clientWidth;
		return () => ro.disconnect();
	});

	$effect(() => {
		const count = rowCount;
		const el = scrollEl;
		const size = estimateRow;
		untrack(() => {
			$virtualizer.setOptions({
				count,
				getScrollElement: () => el,
				estimateSize: () => size,
				gap: GRID_GAP,
				overscan: 4
			});
		});
	});

	let prevCols = 0;
	$effect(() => {
		const cols = columns;
		untrack(() => {
			if (prevCols && prevCols !== cols) {
				$virtualizer.measure();
			}
			prevCols = cols;
		});
	});

	function measureRow(node: HTMLElement, index: number) {
		node.setAttribute('data-index', String(index));
		$virtualizer.measureElement(node);
		return {
			update(newIndex: number) {
				node.setAttribute('data-index', String(newIndex));
				$virtualizer.measureElement(node);
			}
		};
	}

	function rowItems(rowIndex: number): DownloadItem[] {
		const start = rowIndex * columns;
		return items.slice(start, start + columns);
	}

	function videoAction(node: HTMLVideoElement, playing: boolean) {
		const apply = (on: boolean) => {
			if (on) {
				void node.play().catch(() => {});
			} else {
				node.pause();
				node.currentTime = 0;
			}
		};
		apply(playing);
		return {
			update(on: boolean) {
				apply(on);
			}
		};
	}

	function previewable(item: DownloadItem): boolean {
		return item.kind === 'photo' || item.kind === 'video' || item.kind === 'audio';
	}

	function onCell(item: DownloadItem) {
		if (previewable(item) && fileSrc(item.path)) {
			onpreview(item);
			return;
		}
		onopen(item.path);
	}
</script>

<div bind:this={scrollEl} class="min-h-0 flex-1 overflow-y-auto px-3 pb-3">
	<div class="relative w-full" style="height: {$virtualizer.getTotalSize()}px">
		{#each $virtualizer.getVirtualItems() as row (row.index)}
			<div
				class="absolute top-0 left-0 w-full"
				style="transform: translateY({row.start}px)"
				data-index={row.index}
				use:measureRow={row.index}
			>
				<div
					class="grid gap-2"
					style="grid-template-columns: repeat({columns}, minmax(0, 1fr))"
				>
					{#each rowItems(row.index) as item (item.fileId)}
						{@const src = fileSrc(item.path)}
						{@const playing = playingId === item.fileId}
						<article class="overflow-hidden rounded-md border">
							<div class="bg-muted relative aspect-square w-full">
								<button
									type="button"
									class="size-full"
									onclick={() => onCell(item)}
								>
									{#if item.kind === 'photo' && src}
										<img src={src} alt="" class="size-full object-cover" />
									{:else if item.kind === 'video' && src}
										<!-- svelte-ignore a11y_media_has_caption -->
										<video
											src={src}
											class="size-full object-cover"
											muted
											playsinline
											loop={playing}
											preload="metadata"
											use:videoAction={playing}
										></video>
										{#if !playing}
											<Video
												class="pointer-events-none absolute right-1.5 bottom-1.5 size-4 text-white drop-shadow"
											/>
										{/if}
									{:else if item.kind === 'audio'}
										<Music class="text-muted-foreground absolute inset-0 m-auto size-10" />
									{:else}
										<FileText class="text-muted-foreground absolute inset-0 m-auto size-10" />
									{/if}
									<Badge
										variant="secondary"
										class="absolute top-1.5 left-1.5 h-5 px-1.5 text-[10px]"
									>
										{kindLabel(item.kind)}
									</Badge>
								</button>
								{#if item.kind === 'video' && src}
									<button
										type="button"
										class="absolute right-1.5 top-1.5 flex size-7 items-center justify-center rounded-full bg-black/55 text-white"
										onclick={() => onplay(item)}
										aria-label={playing ? '暂停预览' : '格子里播放'}
									>
										{#if playing}
											<Pause class="size-3.5" />
										{:else}
											<Play class="size-3.5" />
										{/if}
									</button>
								{/if}
							</div>
							<div class="space-y-1 p-2">
								<button
									type="button"
									class="w-full truncate text-left text-sm font-medium"
									title={item.fileName}
									onclick={() => onCell(item)}
								>
									{item.fileName}
								</button>
								<p class="text-muted-foreground truncate text-xs" title={chatLabel(item)}>
									{chatLabel(item)}
									{#if formatSize(item.size)}
										· {formatSize(item.size)}
									{/if}
								</p>
							</div>
						</article>
					{/each}
				</div>
			</div>
		{/each}
	</div>
</div>
