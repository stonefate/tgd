<script lang="ts">
	import { createVirtualizer } from '@tanstack/svelte-virtual';
	import { untrack } from 'svelte';

	import { app } from '$lib/app-state.svelte';
	import { commands, events } from '$lib/api';
	import type { MessageItem } from '$lib/bindings';
	import { fallbackMediaSrc, mediaSrc, mediaThumbSrc } from '$lib/media';
	import MediaLightbox, { type MediaPreview } from '$lib/components/media-lightbox.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Search } from '@lucide/svelte';
	import { messageSegments } from '$lib/utils';

	let {
		chatId
	}: {
		chatId: string;
	} = $props();

	const PAGE = 50;

	let query = $state('');
	let appliedQuery = $state('');
	let items = $state<MessageItem[]>([]);
	let hasMore = $state(false);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let scrollEl = $state<HTMLDivElement | null>(null);
	let seq = 0;
	let lightboxIndex = $state<number | null>(null);
	let redownloadingId = $state<number | null>(null);

	const virtualizer = createVirtualizer({
		count: 0,
		getScrollElement: () => scrollEl,
		estimateSize: () => 140,
		gap: 8,
		overscan: 8
	});

	$effect(() => {
		const count = items.length;
		const el = scrollEl;
		const showMedia = app.showMedia;
		untrack(() => {
			$virtualizer.setOptions({
				count,
				getScrollElement: () => el,
				estimateSize: (index) => estimateSize(items[index], showMedia),
				gap: 8
			});
		});
	});

	let prevShowMedia: boolean | null = null;
	$effect(() => {
		const showMedia = app.showMedia;
		untrack(() => {
			if (prevShowMedia !== null && prevShowMedia !== showMedia) {
				$virtualizer.measure();
				remeasureVisible();
			}
			prevShowMedia = showMedia;
		});
	});

	$effect(() => {
		const id = chatId;
		query = '';
		appliedQuery = '';
		void load(id, '', null, true);
	});

	$effect(() => {
		const id = chatId;
		let cancelled = false;
		let unlisten: (() => void) | undefined;
		let timer: ReturnType<typeof setTimeout> | null = null;
		void events.chatIngested.listen((event) => {
			if (cancelled || event.payload.chatId !== id) return;
			if (timer) clearTimeout(timer);
			timer = setTimeout(() => {
				timer = null;
				void refreshFromIngest();
			}, 400);
		}).then((fn) => {
			if (cancelled) fn();
			else unlisten = fn;
		});
		return () => {
			cancelled = true;
			if (timer) clearTimeout(timer);
			unlisten?.();
		};
	});

	function estimateSize(item: MessageItem | undefined, showMedia: boolean): number {
		if (!showMedia || !item?.mediaPath) return 140;
		switch (item.mediaKind) {
			case 'photo':
			case 'video':
				return 280;
			case 'audio':
				return 180;
			default:
				return 140;
		}
	}

	function remeasureVisible() {
		if (!scrollEl) return;
		for (const node of scrollEl.querySelectorAll<HTMLElement>('[data-index]')) {
			$virtualizer.measureElement(node);
		}
	}

	function formatError(err: unknown): string {
		if (typeof err === 'string' && err.trim()) return err;
		if (err && typeof err === 'object' && 'message' in err) {
			const message = (err as { message?: unknown }).message;
			if (typeof message === 'string' && message.trim()) return message;
		}
		if (err instanceof Error && err.message.trim()) return err.message;
		return '加载消息失败';
	}

	function unwrap<T>(result: { status: 'ok'; data: T } | { status: 'error'; error: unknown }): T {
		if (result.status === 'ok') {
			return result.data;
		}
		throw new Error(formatError(result.error));
	}

	function formatTime(unix: string): string {
		const n = Number(unix);
		if (!Number.isFinite(n)) return unix;
		return new Date(n * 1000).toLocaleString('zh-CN', {
			month: '2-digit',
			day: '2-digit',
			hour: '2-digit',
			minute: '2-digit'
		});
	}

	function mediaLabel(kind: string | null): string | null {
		switch (kind) {
			case 'photo':
				return '图片';
			case 'video':
				return '视频';
			case 'audio':
				return '音频';
			case 'document':
				return '文档';
			default:
				return kind;
		}
	}

	function fileSrc(path: string): string | null {
		return mediaSrc(path);
	}

	function thumbSrc(path: string): string | null {
		return mediaThumbSrc(path);
	}

	function previewKind(item: MessageItem): 'photo' | 'video' | 'audio' | null {
		if (!app.showMedia || !item.mediaPath) return null;
		if (item.mediaKind === 'photo' || item.mediaKind === 'video' || item.mediaKind === 'audio') {
			return item.mediaKind;
		}
		return null;
	}

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

	async function load(id: string, q: string, before: number | null, reset: boolean) {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const page = unwrap(await commands.listMessages(id, q || null, before, PAGE));
			if (mine !== seq) return;
			items = reset ? page.items : [...items, ...page.items];
			hasMore = page.hasMore;
			appliedQuery = q;
			if (reset) {
				queueMicrotask(() => scrollEl?.scrollTo({ top: 0 }));
			}
		} catch (err) {
			if (mine !== seq) return;
			error = formatError(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	function search() {
		void load(chatId, query.trim(), null, true);
	}

	function loadMore() {
		if (loading || !hasMore || items.length === 0) return;
		const last = items[items.length - 1];
		if (!last) return;
		void load(chatId, appliedQuery, last.messageId, false);
	}

	async function refreshFromIngest() {
		if (appliedQuery || loading) return;
		const el = scrollEl;
		const atTop = !el || el.scrollTop < 64;
		if (atTop || items.length === 0) {
			void load(chatId, '', null, true);
			return;
		}
		const mine = ++seq;
		try {
			const page = unwrap(await commands.listMessages(chatId, null, null, PAGE));
			if (mine !== seq) return;
			const byId = new Map(items.map((item) => [item.messageId, item]));
			const newer: MessageItem[] = [];
			for (const item of page.items) {
				if (byId.has(item.messageId)) {
					byId.set(item.messageId, item);
				} else {
					newer.push(item);
				}
			}
			if (newer.length === 0) {
				items = items.map((item) => byId.get(item.messageId) ?? item);
				return;
			}
			items = [...newer, ...items.map((item) => byId.get(item.messageId) ?? item)];
		} catch {
			// 后台刷新失败不影响当前列表
		}
	}

	function openFile(path: string | null | undefined) {
		if (!path) return;
		void app.openPath(path);
	}

	function redownloadBusy(item: MessageItem): boolean {
		if (redownloadingId === item.messageId) return true;
		const queued = app.download?.queued ?? [];
		if (
			queued.some(
				(entry) => entry.chatId === item.chatId && entry.messageId === item.messageId
			)
		) {
			return true;
		}
		const fileId = item.mediaFileId;
		if (!fileId) return false;
		if (queued.some((entry) => entry.fileId === fileId)) return true;
		return (app.download?.active ?? []).some((entry) => entry.fileId === fileId);
	}

	async function redownload(item: MessageItem) {
		if (!item.mediaKind || redownloadBusy(item)) return;
		redownloadingId = item.messageId;
		error = null;
		try {
			unwrap(await commands.redownloadMessageMedia(item.chatId, item.messageId));
		} catch (err) {
			error = formatError(err);
		} finally {
			if (redownloadingId === item.messageId) redownloadingId = null;
		}
	}

	const previews = $derived.by(() => {
		const list: MediaPreview[] = [];
		if (!app.showMedia) return list;
		for (const item of items) {
			const kind = previewKind(item);
			if (!kind || !item.mediaPath) continue;
			const src = fileSrc(item.mediaPath);
			if (!src) continue;
			list.push({
				id: `${item.chatId}:${item.messageId}`,
				path: item.mediaPath,
				kind,
				title: mediaLabel(item.mediaKind) || item.text || '媒体',
				src
			});
		}
		return list;
	});

	function openPreview(item: MessageItem) {
		const id = `${item.chatId}:${item.messageId}`;
		const index = previews.findIndex((preview) => preview.id === id);
		if (index >= 0) lightboxIndex = index;
		else openFile(item.mediaPath);
	}

	$effect(() => {
		if (lightboxIndex == null) return;
		if (previews.length === 0) {
			lightboxIndex = null;
			return;
		}
		if (lightboxIndex >= previews.length) lightboxIndex = previews.length - 1;
	});

	function onScroll() {
		const el = scrollEl;
		if (!el) return;
		if (el.scrollTop + el.clientHeight >= el.scrollHeight - 96) {
			loadMore();
		}
	}

	function openLink(event: MouseEvent, href: string) {
		event.preventDefault();
		event.stopPropagation();
		void commands.openUrl(href);
	}
</script>

<div class="flex min-h-0 flex-1 flex-col">
	<div class="flex items-center gap-2 border-b border-border/40 bg-card/70 px-3.5 py-2.5 backdrop-blur-md md:px-5">
		<div class="relative min-w-0 flex-1">
			<Search class="pointer-events-none absolute left-3 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
			<Input
				bind:value={query}
				placeholder="搜索当前会话消息…"
				disabled={loading}
				class="h-8.5 rounded-full border-0 bg-muted/60 pl-8.5 pr-3 text-xs focus-visible:ring-2 focus-visible:ring-primary/40"
				onkeydown={(event) => {
					if (event.key === 'Enter') {
						event.preventDefault();
						search();
					}
				}}
			/>
		</div>
		<Button
			variant="outline"
			size="sm"
			class="h-8.5 rounded-full px-3.5 text-xs font-medium"
			onclick={search}
			disabled={loading}
		>
			搜索
		</Button>
	</div>

	{#if error}
		<div class="px-3.5 py-2 text-xs font-medium text-destructive md:px-5">{error}</div>
	{/if}

	{#if items.length === 0}
		<div class="flex flex-1 flex-col items-center justify-center p-8 text-center text-sm text-muted-foreground">
			{#if loading}
				<p>加载中…</p>
			{:else if appliedQuery}
				<p>没有匹配的消息。</p>
			{:else}
				<p>还没有入库的消息。勾选「文本」并回爬后才会出现。</p>
			{/if}
		</div>
	{:else}
		<div bind:this={scrollEl} class="min-h-0 flex-1 overflow-y-auto px-3 py-2" onscroll={onScroll}>
			<div class="relative w-full" style="height: {$virtualizer.getTotalSize()}px">
				{#each $virtualizer.getVirtualItems() as row (items[row.index]?.messageId ?? row.index)}
					{@const item = items[row.index]}
					{#if item}
						{@const kind = previewKind(item)}
						{@const src = item.mediaPath ? fileSrc(item.mediaPath) : null}
						{@const preview =
							kind === 'photo' && item.mediaPath
								? (thumbSrc(item.mediaPath) ?? src)
								: src}
						<div
							class="absolute top-0 left-0 w-full px-0.5 py-1.5"
							style="transform: translateY({row.start}px)"
							data-index={row.index}
							use:measureRow={row.index}
						>
							<div class="space-y-2 rounded-2xl border border-border/40 bg-muted/20 p-3 transition-colors hover:border-primary/20 hover:bg-muted/30">
								<div class="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
									<span class="font-semibold text-foreground">
										{item.sender || '未知'}
									</span>
									<span class="text-muted-foreground/40">·</span>
									<span class="tabular-nums">{formatTime(item.dateUnix)}</span>
									{#if mediaLabel(item.mediaKind)}
										<Badge variant="secondary" class="rounded-full border-0 bg-secondary/80 px-2 py-0.5 text-[10px] font-normal">
											{mediaLabel(item.mediaKind)}
										</Badge>
									{/if}
								</div>
								{#if kind && (kind === 'photo' ? preview : src)}
									{#if kind === 'photo' && preview}
										<button
											type="button"
											class="block max-w-full overflow-hidden rounded-2xl transition-transform active:scale-[0.99]"
											onclick={() => openPreview(item)}
										>
											<img
												src={preview}
												alt=""
												class="max-h-52 max-w-full rounded-2xl object-contain"
												onerror={(event) => fallbackMediaSrc(event.currentTarget, src)}
											/>
										</button>
									{:else if kind === 'video'}
										<!-- svelte-ignore a11y_media_has_caption -->
										<video
											src={src}
											class="max-h-52 w-full rounded-2xl"
											controls
											preload="metadata"
										></video>
									{:else}
										<audio src={src} class="w-full" controls preload="metadata"></audio>
									{/if}
								{/if}
								{#if item.mediaKind}
									<div class="flex flex-wrap items-center gap-3 pt-0.5">
										{#if item.mediaPath && kind}
											<button
												type="button"
												class="text-xs font-medium text-primary underline underline-offset-2 hover:opacity-80"
												onclick={() => openPreview(item)}
											>
												预览
											</button>
										{/if}
										{#if item.mediaPath}
											<button
												type="button"
												class="text-xs font-medium text-primary underline underline-offset-2 hover:opacity-80"
												onclick={() => openFile(item.mediaPath)}
											>
												打开文件
											</button>
										{/if}
										<button
											type="button"
											class="text-xs font-medium text-primary underline underline-offset-2 hover:opacity-80 disabled:opacity-50"
											onclick={() => void redownload(item)}
											disabled={redownloadBusy(item) || app.busy}
										>
											{redownloadBusy(item) ? '已加入队列…' : '重新下载'}
										</button>
									</div>
								{/if}
								<p class="line-clamp-4 leading-relaxed break-words whitespace-pre-wrap text-sm text-foreground/90">
									<span>
										{#if item.text}
											{#each messageSegments(item.text, item.links ?? []) as seg, index (index)}
												{#if seg.href}
													<a
														href={seg.href}
														class="text-primary underline underline-offset-2 hover:opacity-80"
														onclick={(event) => openLink(event, seg.href ?? '')}
													>
														{seg.text}
													</a>
												{:else}
													{seg.text}
												{/if}
											{/each}
										{:else}
											<span class="text-muted-foreground italic">（无文字）</span>
										{/if}
									</span>
								</p>
							</div>
						</div>
					{/if}
				{/each}
			</div>
		</div>
		{#if hasMore}
			<div class="flex justify-center py-3">
				<Button variant="outline" size="sm" class="rounded-full px-5 text-xs" onclick={loadMore} disabled={loading}>
					{loading ? '加载中…' : '加载更早消息'}
				</Button>
			</div>
		{/if}
	{/if}
</div>

{#if lightboxIndex != null && previews[lightboxIndex]}
	<MediaLightbox
		items={previews}
		index={lightboxIndex}
		onclose={() => (lightboxIndex = null)}
		onindex={(next) => (lightboxIndex = next)}
		onopen={(path) => openFile(path)}
	/>
{/if}
