<script lang="ts">
	import { convertFileSrc } from '@tauri-apps/api/core';
	import { FolderOpen, HardDriveDownload, Pause, Play, RefreshCw } from '@lucide/svelte';

	import { app } from '$lib/app-state.svelte';
	import type { DownloadItem, DownloadUsage, MediaKind } from '$lib/bindings';
	import { commands } from '$lib/bindings';
	import DownloadGrid from '$lib/components/download-grid.svelte';
	import MediaLightbox, { type MediaPreview } from '$lib/components/media-lightbox.svelte';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';

	type KindFilter = 'all' | MediaKind;

	const KIND_FILTERS: { id: KindFilter; label: string }[] = [
		{ id: 'all', label: '全部' },
		{ id: 'photo', label: '图片' },
		{ id: 'video', label: '视频' },
		{ id: 'audio', label: '音频' },
		{ id: 'document', label: '文档' }
	];

	const UNKNOWN_CHAT = '__unknown__';

	let filter = $state('');
	let kindFilter = $state<KindFilter>('all');
	let chatFilter = $state('all');
	let items = $state<DownloadItem[]>([]);
	let usage = $state<DownloadUsage | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let lastDownloaded = $state<number | null>(null);
	let seq = 0;
	let lightboxIndex = $state<number | null>(null);
	let playingId = $state<string | null>(null);

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

	$effect(() => {
		const n = app.download?.downloaded ?? 0;
		if (lastDownloaded === n) return;
		lastDownloaded = n;
		void load();
	});

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
		try {
			return convertFileSrc(path);
		} catch {
			return null;
		}
	}

	async function load() {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const [nextItems, nextUsage] = await Promise.all([
				commands.listDownloads(),
				commands.getDownloadUsage()
			]);
			items = unwrap(nextItems);
			usage = unwrap(nextUsage);
		} catch (err) {
			if (mine !== seq) return;
			error = formatError(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	const active = $derived(
		app.download && app.download.phase !== 'idle' ? app.download : null
	);
	const filteredEmptyHint = $derived(
		!!filter.trim() || kindFilter !== 'all' || chatFilter !== 'all'
	);

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

<div class="flex h-full min-h-0 flex-col">
	<section class="space-y-2 border-b px-4 py-3">
		<div class="flex items-center justify-between gap-2">
			<div class="flex items-center gap-2 text-sm font-medium">
				<HardDriveDownload class="size-4" />
				进行中
			</div>
			<Button
				variant="outline"
				size="xs"
				onclick={() => void app.setDownloadPaused(!app.download?.paused)}
			>
				{#if app.download?.paused}
					<Play class="size-3.5" />
					继续
				{:else}
					<Pause class="size-3.5" />
					暂停全部
				{/if}
			</Button>
		</div>
		{#if active || app.download?.paused}
			<div class="space-y-2 text-sm">
				<p>{app.syncLabel}</p>
				{#each app.download?.active ?? [] as item (item.fileId)}
					{@const percent = progressPercent(item.bytes, item.total)}
					<div class="space-y-1">
						<div class="flex items-center justify-between gap-2">
							<p class="min-w-0 truncate text-xs">
								{kindLabel(item.kind)} · {item.fileName}
							</p>
							<Button
								variant="ghost"
								size="xs"
								class="shrink-0"
								onclick={() => void app.cancelDownload(item.fileId)}
							>
								取消
							</Button>
						</div>
						<div class="bg-muted h-1.5 overflow-hidden rounded-full">
							<div
								class="bg-primary h-full rounded-full {percent == null
									? 'w-1/3 animate-pulse'
									: ''}"
								style={percent != null ? `width: ${percent}%` : undefined}
							></div>
						</div>
						<p class="text-muted-foreground text-[11px]">
							{formatSize(item.bytes) || '0 B'}
							{#if item.total}
								/ {formatSize(item.total)}{#if percent != null}
									· {percent}%{/if}
							{/if}
						</p>
					</div>
				{/each}
			</div>
		{:else}
			<p class="text-muted-foreground text-sm">当前没有下载任务</p>
		{/if}
	</section>

	<div class="space-y-2 border-b px-4 py-2">
		<div class="flex items-center gap-2">
			<Input bind:value={filter} placeholder="搜索文件或会话" class="h-8" />
			<select
				class="border-input bg-background h-8 min-w-28 rounded-md border px-2 text-sm"
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
				variant="outline"
				size="icon-sm"
				onclick={() => void load()}
				disabled={loading}
				aria-label="刷新下载列表"
			>
				<RefreshCw class="size-4" />
			</Button>
			<Button
				variant="outline"
				size="icon-sm"
				onclick={() => void app.openDownloadDir()}
				aria-label="打开下载目录"
			>
				<FolderOpen class="size-4" />
			</Button>
		</div>
		<div class="flex flex-wrap gap-1">
			{#each KIND_FILTERS as option (option.id)}
				<Button
					variant={kindFilter === option.id ? 'default' : 'outline'}
					size="xs"
					onclick={() => (kindFilter = option.id)}
				>
					{option.label}
				</Button>
			{/each}
		</div>
		{#if usage}
			<div class="text-muted-foreground space-y-0.5 text-xs">
				<p>
					占用 {formatSize(usage.total) || '0 B'}
					{#if Number(usage.parts) > 0}
						（未完成 {formatSize(usage.parts)}）
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
		<p class="text-destructive px-4 py-2 text-sm">{error}</p>
	{/if}

	{#if filtered.length === 0}
		<p class="text-muted-foreground px-4 py-6 text-sm">
			{#if loading}
				加载中…
			{:else if filteredEmptyHint}
				没有匹配的文件。
			{:else}
				还没有已下载的媒体。
			{/if}
		</p>
	{:else}
		<div class="text-muted-foreground px-4 py-2 text-xs">
			{#if filtered.length === items.length}
				已完成 {items.length} 个
			{:else}
				已完成 {filtered.length} / {items.length} 个
			{/if}
		</div>
		<DownloadGrid
			items={filtered}
			{fileSrc}
			{kindLabel}
			{chatLabel}
			{formatSize}
			{playingId}
			onpreview={openPreview}
			onopen={openFile}
			onplay={togglePlay}
		/>
	{/if}
</div>

{#if lightboxIndex != null && previews[lightboxIndex]}
	<MediaLightbox
		items={previews}
		index={lightboxIndex}
		onclose={() => (lightboxIndex = null)}
		onindex={(next) => (lightboxIndex = next)}
		onopen={openFile}
	/>
{/if}
