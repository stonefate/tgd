<script lang="ts">
	import { onDestroy } from 'svelte';
	import { FolderOpen, Pause, Play, RefreshCw, Search } from '@lucide/svelte';

	import { app } from '$lib/app-state.svelte';
	import { commands } from '$lib/api';
	import type { DownloadItem, DownloadUsage, MediaKind } from '$lib/bindings';
	import { DownloadRateTracker, formatRateLine } from '$lib/download-rate';
	import { mediaSrc, mediaThumbSrc } from '$lib/media';
	import { isTauri } from '$lib/runtime';
	import DownloadGrid from '$lib/components/download-grid.svelte';
	import MediaLightbox, { type MediaPreview } from '$lib/components/media-lightbox.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { cn } from '$lib/utils';

	type KindFilter = 'all' | MediaKind;
	type PageTab = 'active' | 'done';

	const KIND_FILTERS: { id: KindFilter; label: string }[] = [
		{ id: 'all', label: '全部' },
		{ id: 'photo', label: '图片' },
		{ id: 'video', label: '视频' },
		{ id: 'audio', label: '音频' },
		{ id: 'document', label: '文档' }
	];

	const TABS: { id: PageTab; label: string }[] = [
		{ id: 'active', label: '正在下载' },
		{ id: 'done', label: '已下载' }
	];

	const UNKNOWN_CHAT = '__unknown__';

	let tab = $state<PageTab>('done');
	let hadWork = false;
	let filter = $state('');
	let kindFilter = $state<KindFilter>('all');
	let chatFilter = $state('all');
	let items = $state<DownloadItem[]>([]);
	let usage = $state<DownloadUsage | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let lastDownloaded = $state<number | null>(null);
	let seq = 0;
	const REFRESH_DEBOUNCE_MS = 400;
	let debounceTimer: ReturnType<typeof setTimeout> | null = null;
	let loadPending = false;
	let lightboxIndex = $state<number | null>(null);
	let playingId = $state<string | null>(null);
	const rateTracker = new DownloadRateTracker();
	let rateLine = $state<Record<string, string>>({});

	function chatLabel(item: {
		alias?: string | null;
		chatTitle?: string | null;
		chatId?: string | null;
	}): string {
		return item.alias?.trim() || item.chatTitle?.trim() || item.chatId || '未知会话';
	}

	const chatOptions = $derived.by(() => {
		const map = new Map<string, string>();
		for (const item of items) {
			if (!item.chatId) continue;
			if (!map.has(item.chatId)) {
				map.set(item.chatId, chatLabel(item));
			}
		}
		return [...map.entries()]
			.map(([id, label]) => ({ id, label }))
			.sort((a, b) => a.label.localeCompare(b.label, 'zh-CN'));
	});

	const hasUnknownChat = $derived(items.some((item) => !item.chatId));

	const filtered = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		return items.filter((item) => {
			if (kindFilter !== 'all' && item.kind !== kindFilter) return false;
			if (chatFilter === UNKNOWN_CHAT) {
				if (item.chatId) return false;
			} else if (chatFilter !== 'all' && item.chatId !== chatFilter) {
				return false;
			}
			if (!q) return true;
			const chat = (item.alias || item.chatTitle || '').toLowerCase();
			return (
				item.fileName.toLowerCase().includes(q) ||
				chat.includes(q) ||
				(item.chatId?.includes(q) ?? false)
			);
		});
	});

	$effect(() => {
		if (chatFilter === 'all') return;
		if (chatFilter === UNKNOWN_CHAT && hasUnknownChat) return;
		if (chatOptions.some((option) => option.id === chatFilter)) return;
		chatFilter = 'all';
	});

	function clearDebounce() {
		if (debounceTimer == null) return;
		clearTimeout(debounceTimer);
		debounceTimer = null;
	}

	function scheduleLoad() {
		clearDebounce();
		debounceTimer = setTimeout(() => {
			debounceTimer = null;
			if (loading) {
				loadPending = true;
				return;
			}
			void load();
		}, REFRESH_DEBOUNCE_MS);
	}

	$effect(() => {
		const n = app.download?.downloaded ?? 0;
		if (lastDownloaded === n) return;
		const first = lastDownloaded === null;
		lastDownloaded = n;
		if (first) void load();
		else scheduleLoad();
	});

	onDestroy(clearDebounce);

	function unwrap<T>(result: { status: 'ok'; data: T } | { status: 'error'; error: unknown }): T {
		if (result.status === 'ok') return result.data;
		throw new Error(formatError(result.error));
	}

	function formatError(err: unknown): string {
		if (typeof err === 'string' && err.trim()) return err;
		if (err && typeof err === 'object' && 'message' in err) {
			const message = (err as { message?: unknown }).message;
			if (typeof message === 'string' && message.trim()) return message;
		}
		if (err instanceof Error && err.message.trim()) return err.message;
		return '加载下载列表失败';
	}

	function kindLabel(kind: string | null | undefined): string {
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
				return kind || '文件';
		}
	}

	function formatSize(raw: string | null | undefined): string {
		const n = Number(raw);
		if (!Number.isFinite(n) || n < 0) return '';
		if (n < 1024) return `${Math.round(n)} B`;
		if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
		if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
		return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
	}

	function kindUsageLine(kinds: DownloadUsage['kinds']): string {
		return (
			[
				['图片', kinds.photo],
				['视频', kinds.video],
				['音频', kinds.audio],
				['文档', kinds.document],
				['其他', kinds.other]
			] as const
		)
			.filter(([, n]) => Number(n) > 0)
			.map(([label, n]) => `${label} ${formatSize(n)}`)
			.join(' · ');
	}

	function usageChatLabel(chat: DownloadUsage['chats'][number]): string {
		return chat.alias?.trim() || chat.chatTitle?.trim() || chat.chatId || '未知会话';
	}

	function progressPercent(bytes: string, total: string | null): number | null {
		const done = Number(bytes);
		const all = Number(total);
		if (!Number.isFinite(done) || !Number.isFinite(all) || all <= 0) return null;
		return Math.max(0, Math.min(100, Math.round((done / all) * 100)));
	}

	function fileSrc(path: string): string | null {
		return mediaSrc(path);
	}

	function thumbSrc(path: string): string | null {
		return mediaThumbSrc(path);
	}

	async function load() {
		clearDebounce();
		loadPending = false;
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const [nextItems, nextUsage] = await Promise.all([
				commands.listDownloads(),
				commands.getDownloadUsage()
			]);
			if (mine !== seq) return;
			items = unwrap(nextItems);
			usage = unwrap(nextUsage);
		} catch (err) {
			if (mine !== seq) return;
			error = formatError(err);
		} finally {
			if (mine === seq) {
				loading = false;
				if (loadPending) {
					loadPending = false;
					void load();
				}
			}
		}
	}

	const activeCount = $derived(app.download?.active.length ?? 0);
	const queued = $derived(app.download?.queued ?? []);
	const queuedCount = $derived(queued.length);
	const hasWork = $derived.by(() => {
		const progress = app.download;
		if (!progress) return false;
		if (progress.paused || progress.active.length > 0 || progress.queued.length > 0) return true;
		return progress.phase === 'backfill' || progress.phase === 'floodWait';
	});
	// 回补新类型等会把 phase 打成 idle，但 active 里仍有正在下的文件。
	const showActivePane = $derived(hasWork);
	const filteredEmptyHint = $derived(
		!!filter.trim() || kindFilter !== 'all' || chatFilter !== 'all'
	);

	function setTab(next: PageTab) {
		tab = next;
		if (next !== 'done') {
			lightboxIndex = null;
			playingId = null;
		}
	}

	// 从空闲变为有任务时切到「正在下载」；用户手动切走后不强制拉回
	$effect(() => {
		const work = hasWork;
		if (work && !hadWork) setTab('active');
		hadWork = work;
	});

	$effect(() => {
		const progress = app.download;
		const next = rateTracker.update(progress?.active ?? [], !!progress?.paused);
		const lines: Record<string, string> = {};
		for (const [id, sample] of next) {
			lines[id] = formatRateLine(sample);
		}
		rateLine = lines;
	});

	function openFile(path: string) {
		void app.openPath(path);
	}

	function previewable(item: DownloadItem): boolean {
		return item.kind === 'photo' || item.kind === 'video' || item.kind === 'audio';
	}

	const previews = $derived.by(() => {
		const list: MediaPreview[] = [];
		for (const item of filtered) {
			if (!previewable(item)) continue;
			const src = fileSrc(item.path);
			if (!src) continue;
			list.push({
				id: item.fileId,
				path: item.path,
				kind: item.kind,
				title: item.fileName,
				src
			});
		}
		return list;
	});

	function openPreview(item: DownloadItem) {
		playingId = null;
		const index = previews.findIndex((preview) => preview.id === item.fileId);
		if (index >= 0) lightboxIndex = index;
		else openFile(item.path);
	}

	function togglePlay(item: DownloadItem) {
		playingId = playingId === item.fileId ? null : item.fileId;
	}

	$effect(() => {
		if (lightboxIndex == null) return;
		if (previews.length === 0) {
			lightboxIndex = null;
			return;
		}
		if (lightboxIndex >= previews.length) lightboxIndex = previews.length - 1;
	});
</script>

<div class="flex h-full min-h-0 flex-col bg-background md:p-3">
	<div class="flex min-h-0 flex-1 flex-col bg-card overflow-hidden md:rounded-3xl md:border md:border-border/50 md:shadow-xs">
		<div class="flex items-center justify-between border-b border-border/40 bg-card/70 px-3.5 py-2.5 backdrop-blur-md md:px-5">
			<div class="inline-flex rounded-full bg-muted/70 p-1">
				{#each TABS as option (option.id)}
					{@const active = tab === option.id}
					<button
						type="button"
						class={cn(
							'flex items-center gap-1.5 rounded-full px-4 py-1 text-xs font-medium transition-all m3-press',
							active
								? 'bg-card text-foreground shadow-2xs font-semibold'
								: 'text-muted-foreground hover:text-foreground'
						)}
						onclick={() => setTab(option.id)}
					>
						<span>{option.label}</span>
						{#if option.id === 'active' && activeCount + queuedCount > 0}
							<span class="rounded-full bg-primary/15 px-1.5 py-0.2 text-[10px] font-semibold text-primary tabular-nums">
								{activeCount + queuedCount}
							</span>
						{/if}
					</button>
				{/each}
			</div>
			{#if tab === 'active'}
				<Button
					variant="outline"
					size="xs"
					class="h-7.5 rounded-full px-3 text-xs"
					onclick={() => void app.setDownloadPaused(!app.download?.paused)}
				>
					{#if app.download?.paused}
						<Play class="size-3.5 fill-current" />
						继续
					{:else}
						<Pause class="size-3.5" />
						暂停全部
					{/if}
				</Button>
			{/if}
		</div>

		{#if tab === 'active'}
			<section class="flex min-h-0 flex-1 flex-col overflow-hidden">
				<div class="min-h-0 flex-1 overflow-y-auto px-3.5 py-3 md:px-5">
					{#if showActivePane}
						<div class="space-y-3 text-sm">
							<div class="rounded-2xl border border-border/40 bg-muted/20 px-3.5 py-2 text-xs font-medium text-foreground">
								{app.syncLabel}
							</div>
							{#each app.download?.active ?? [] as item (item.fileId)}
								{@const percent = progressPercent(item.bytes, item.total)}
								<div class="space-y-2 rounded-2xl border border-border/40 bg-card p-3 shadow-2xs">
									<div class="flex items-center justify-between gap-2">
										<p class="min-w-0 truncate text-xs font-medium text-foreground">
											<span class="text-primary font-semibold">[{kindLabel(item.kind)}]</span> {item.fileName}
										</p>
										<Button
											variant="ghost"
											size="xs"
											class="h-6 shrink-0 rounded-full px-2 text-xs text-muted-foreground hover:text-destructive hover:bg-destructive/10"
											onclick={() => void app.cancelDownload(item.fileId)}
										>
											取消
										</Button>
									</div>
									<div class="h-2 overflow-hidden rounded-full bg-muted">
										<div
											class="h-full rounded-full bg-primary transition-all duration-300 {percent == null
												? 'w-1/3 animate-pulse'
												: ''}"
											style={percent != null ? `width: ${percent}%` : undefined}
										></div>
									</div>
									<p class="text-[11px] text-muted-foreground">
										{formatSize(item.bytes) || '0 B'}
										{#if item.total}
											/ {formatSize(item.total)}{#if percent != null}
												· {percent}%{/if}
										{/if}
										{#if rateLine[item.fileId]}
											· {rateLine[item.fileId]}{/if}
									</p>
								</div>
							{/each}
							{#if queuedCount > 0}
								<p class="pt-2 text-xs font-medium text-muted-foreground">
									接下来 {queuedCount} 个
								</p>
								<ol class="space-y-1.5">
									{#each queued as item, index (item.fileId)}
										<li class="flex items-center justify-between gap-2 rounded-xl bg-muted/30 px-3 py-2 text-xs text-muted-foreground">
											<span class="min-w-0 truncate">
												{index + 1}. {kindLabel(item.kind)} · {item.fileName}
											</span>
											<Button
												variant="ghost"
												size="xs"
												class="h-6 shrink-0 rounded-full px-2 text-xs hover:text-destructive"
												onclick={() => void app.cancelDownload(item.fileId)}
											>
												取消
											</Button>
										</li>
									{/each}
								</ol>
							{/if}
						</div>
					{:else}
						<div class="flex flex-1 items-center justify-center py-16 text-center text-sm text-muted-foreground">
							当前没有正在进行的下载任务
						</div>
					{/if}
				</div>
			</section>
		{:else}
			<div class="space-y-2.5 border-b border-border/40 px-3.5 py-3 md:px-5">
				<div class="flex flex-wrap items-center gap-2">
					<div class="relative min-w-0 flex-1">
						<Search class="pointer-events-none absolute left-3 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
						<Input
							bind:value={filter}
							placeholder="搜索文件或会话…"
							class="h-9 rounded-full border-0 bg-muted/60 pl-8.5 pr-3 text-xs focus-visible:ring-2 focus-visible:ring-primary/40"
						/>
					</div>
					<select
						class="h-9 min-w-0 rounded-full border-0 bg-muted/60 px-3 text-xs text-foreground md:min-w-32"
						bind:value={chatFilter}
						aria-label="按会话筛选"
					>
						<option value="all">全部会话</option>
						{#each chatOptions as option (option.id)}
							<option value={option.id}>{option.label}</option>
						{/each}
						{#if hasUnknownChat}
							<option value={UNKNOWN_CHAT}>未知会话</option>
						{/if}
					</select>
					<Button
						variant="ghost"
						size="icon-sm"
						class="size-9 shrink-0 rounded-full hover:bg-muted/70"
						onclick={() => void load()}
						disabled={loading}
						aria-label="刷新下载列表"
					>
						<RefreshCw class="size-4" />
					</Button>
					{#if isTauri()}
						<Button
							variant="outline"
							size="icon-sm"
							class="size-9 shrink-0 rounded-full"
							onclick={() => void app.openDownloadDir()}
							aria-label="打开下载目录"
						>
							<FolderOpen class="size-4" />
						</Button>
					{/if}
				</div>
				<div class="flex flex-wrap items-center gap-1.5">
					{#each KIND_FILTERS as option (option.id)}
						{@const active = kindFilter === option.id}
						<button
							type="button"
							class={cn(
								'rounded-full px-3 py-1 text-xs font-medium transition-all m3-press',
								active
									? 'bg-primary text-primary-foreground shadow-2xs'
									: 'bg-muted/60 text-muted-foreground hover:bg-muted hover:text-foreground'
							)}
							onclick={() => (kindFilter = option.id)}
						>
							{option.label}
						</button>
					{/each}
				</div>
				{#if usage}
					<div class="space-y-1 rounded-2xl border border-border/40 bg-muted/30 p-2.5 text-xs text-muted-foreground">
						<p class="font-medium text-foreground">
							占用 {formatSize(usage.total) || '0 B'}
							{#if Number(usage.parts) > 0}
								<span class="text-primary font-normal">（未完成 {formatSize(usage.parts)}）</span>
							{/if}
						</p>
						{#if kindUsageLine(usage.kinds)}
							<p>{kindUsageLine(usage.kinds)}</p>
						{/if}
						{#if usage.chats.length > 0}
							<p
								class="truncate"
								title={usage.chats
									.map((chat) => `${usageChatLabel(chat)} ${formatSize(chat.bytes)}`)
									.join(' · ')}
							>
								{usage.chats
									.slice(0, 6)
									.map((chat) => `${usageChatLabel(chat)} ${formatSize(chat.bytes)}`)
									.join(' · ')}
								{#if usage.chats.length > 6}
									· 共 {usage.chats.length} 个会话
								{/if}
							</p>
						{/if}
					</div>
				{/if}
			</div>

			{#if error}
				<div class="px-3.5 py-2 text-xs font-medium text-destructive md:px-5">{error}</div>
			{/if}

			{#if filtered.length === 0}
				<p class="px-4 py-8 text-center text-sm text-muted-foreground">
					{#if loading}
						加载中…
					{:else if filteredEmptyHint}
						没有匹配的文件。
					{:else}
						还没有已下载的媒体。
					{/if}
				</p>
			{:else}
				<div class="px-3.5 py-2 text-xs text-muted-foreground md:px-5">
					{#if filtered.length === items.length}
						已完成 {items.length} 个
					{:else}
						已完成 {filtered.length} / {items.length} 个
					{/if}
				</div>
				<DownloadGrid
					items={filtered}
					{fileSrc}
					{thumbSrc}
					{kindLabel}
					{chatLabel}
					{formatSize}
					{playingId}
					onpreview={openPreview}
					onopen={openFile}
					onplay={togglePlay}
				/>
			{/if}
		{/if}
	</div>
</div>

{#if tab === 'done' && lightboxIndex != null && previews[lightboxIndex]}
	<MediaLightbox
		items={previews}
		index={lightboxIndex}
		onclose={() => (lightboxIndex = null)}
		onindex={(next) => (lightboxIndex = next)}
		onopen={openFile}
	/>
{/if}
