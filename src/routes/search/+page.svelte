<script lang="ts">
	import { goto } from '$app/navigation';

	import { app } from '$lib/app-state.svelte';
	import { httpPath } from '$lib/runtime';
	import type { MessageItem, SearchCursor } from '$lib/bindings';
	import { commands } from '$lib/api';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Search } from '@lucide/svelte';
	import { displayTitle, messageSegments } from '$lib/utils';

	const PAGE = 50;

	let query = $state('');
	let appliedQuery = $state('');
	let items = $state<MessageItem[]>([]);
	let hasMore = $state(false);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let seq = 0;

	function formatError(err: unknown): string {
		if (typeof err === 'string' && err.trim()) return err;
		if (err && typeof err === 'object' && 'message' in err) {
			const message = (err as { message?: unknown }).message;
			if (typeof message === 'string' && message.trim()) return message;
		}
		if (err instanceof Error && err.message.trim()) return err.message;
		return '搜索失败';
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

	function chatLabel(chatId: string): string {
		const chat = app.chats.find((item) => item.id === chatId);
		return chat ? displayTitle(chat) : chatId;
	}

	function cursorOf(item: MessageItem): SearchCursor {
		return {
			dateUnix: item.dateUnix,
			chatId: item.chatId,
			messageId: item.messageId
		};
	}

	async function load(q: string, cursor: SearchCursor | null, reset: boolean) {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const page = unwrap(await commands.searchMessages(q, cursor, PAGE));
			if (mine !== seq) return;
			items = reset ? page.items : [...items, ...page.items];
			hasMore = page.hasMore;
			appliedQuery = q;
		} catch (err) {
			if (mine !== seq) return;
			error = formatError(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	function search() {
		const q = query.trim();
		if (!q) {
			items = [];
			hasMore = false;
			appliedQuery = '';
			error = null;
			return;
		}
		void load(q, null, true);
	}

	function loadMore() {
		if (loading || !hasMore || items.length === 0) return;
		const last = items[items.length - 1];
		if (!last) return;
		void load(appliedQuery, cursorOf(last), false);
	}

	function openChat(chatId: string) {
		app.selectChat(chatId);
		void goto(httpPath('/'));
	}

	function openLink(event: MouseEvent, href: string) {
		event.preventDefault();
		event.stopPropagation();
		void commands.openUrl(href);
	}
</script>

<div class="flex h-full min-h-0 flex-col bg-background md:p-3">
	<div class="flex min-h-0 flex-1 flex-col bg-card overflow-hidden md:rounded-3xl md:border md:border-border/50 md:shadow-xs">
		<div class="border-b border-border/40 bg-card/70 px-3.5 py-3 backdrop-blur-md md:px-5">
			<div class="flex items-center gap-2">
				<div class="relative min-w-0 flex-1">
					<Search class="pointer-events-none absolute left-3.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
					<Input
						bind:value={query}
						placeholder="跨会话搜索已入库消息…"
						disabled={loading}
						class="h-10 rounded-full border-0 bg-muted/60 pl-9.5 pr-4 text-xs focus-visible:ring-2 focus-visible:ring-primary/40 md:text-sm"
						onkeydown={(event) => {
							if (event.key === 'Enter') {
								event.preventDefault();
								search();
							}
						}}
					/>
				</div>
				<Button
					class="h-10 rounded-full px-5 text-xs font-medium shadow-2xs md:text-sm"
					onclick={search}
					disabled={loading || !query.trim()}
				>
					{loading ? '搜索中…' : '搜索'}
				</Button>
			</div>
		</div>

		{#if error}
			<div class="px-3.5 py-2 text-xs font-medium text-destructive md:px-5">{error}</div>
		{/if}

		{#if items.length === 0}
			<div class="flex flex-1 flex-col items-center justify-center p-8 text-center text-sm text-muted-foreground">
				<div class="mb-3 flex size-14 items-center justify-center rounded-3xl bg-muted/40 text-muted-foreground/60">
					<Search class="size-7" />
				</div>
				{#if loading}
					<p>正在跨会话检索消息…</p>
				{:else if appliedQuery}
					<p>没有匹配「{appliedQuery}」的消息。</p>
				{:else}
					<p>在已入库的群组 / 频道消息里搜索。只收了文本的会话才会有正文。</p>
				{/if}
			</div>
		{:else}
			<div class="min-h-0 flex-1 overflow-y-auto px-3.5 py-3 md:px-5">
				<ul class="space-y-2.5">
					{#each items as item (`${item.chatId}:${item.messageId}`)}
						<li class="space-y-2 rounded-2xl border border-border/40 bg-muted/20 p-3.5 transition-all hover:border-primary/30 hover:bg-muted/35 hover:shadow-2xs">
							<div class="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
								<button
									type="button"
									class="font-semibold text-primary hover:underline underline-offset-2"
									onclick={() => openChat(item.chatId)}
								>
									{chatLabel(item.chatId)}
								</button>
								<span class="text-muted-foreground/40">·</span>
								<span class="font-medium text-foreground">{item.sender || '未知'}</span>
								<span class="text-muted-foreground/40">·</span>
								<span class="tabular-nums">{formatTime(item.dateUnix)}</span>
								{#if mediaLabel(item.mediaKind)}
									<Badge variant="secondary" class="rounded-full border-0 bg-secondary/80 px-2 py-0.5 text-[10px] font-normal">
										{mediaLabel(item.mediaKind)}
									</Badge>
								{/if}
							</div>
							<p class="line-clamp-4 text-sm leading-relaxed text-foreground/90 break-words whitespace-pre-wrap">
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
						</li>
					{/each}
				</ul>
				{#if hasMore}
					<div class="flex justify-center py-4">
						<Button variant="outline" size="sm" class="rounded-full px-5 text-xs" onclick={loadMore} disabled={loading}>
							{loading ? '加载中…' : '加载更早消息'}
						</Button>
					</div>
				{/if}
			</div>
		{/if}
	</div>
</div>
