<script lang="ts">
	import { goto } from '$app/navigation';

	import { app } from '$lib/app-state.svelte';
	import type { MessageItem, SearchCursor } from '$lib/bindings';
	import { commands } from '$lib/bindings';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
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
		void goto('/');
	}

	function openLink(event: MouseEvent, href: string) {
		event.preventDefault();
		event.stopPropagation();
		void commands.openUrl(href);
	}
</script>

<div class="flex h-full min-h-0 flex-col">
	<div class="flex gap-2 px-4 py-3">
		<Input
			bind:value={query}
			placeholder="跨会话搜索已入库消息"
			disabled={loading}
			onkeydown={(event) => {
				if (event.key === 'Enter') {
					event.preventDefault();
					search();
				}
			}}
		/>
		<Button variant="outline" onclick={search} disabled={loading}>搜索</Button>
	</div>

	{#if error}
		<p class="px-4 pb-2 text-sm text-destructive">{error}</p>
	{/if}

	{#if items.length === 0}
		<p class="px-4 text-sm text-muted-foreground">
			{#if loading}
				搜索中…
			{:else if appliedQuery}
				没有匹配的消息。
			{:else}
				在已入库的群组 / 频道消息里搜索。只收了文本的会话才会有正文。
			{/if}
		</p>
	{:else}
		<div class="min-h-0 flex-1 overflow-y-auto px-4 pb-4">
			<ul class="divide-y">
				{#each items as item (`${item.chatId}:${item.messageId}`)}
					<li class="space-y-1 px-1 py-3">
						<div class="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
							<button
								type="button"
								class="font-medium text-foreground hover:text-primary"
								onclick={() => openChat(item.chatId)}
							>
								{chatLabel(item.chatId)}
							</button>
							<span>{item.sender || '未知'}</span>
							<span>{formatTime(item.dateUnix)}</span>
							{#if mediaLabel(item.mediaKind)}
								<Badge variant="secondary" class="h-5 px-1.5 text-[10px]">
									{mediaLabel(item.mediaKind)}
								</Badge>
							{/if}
						</div>
						<p class="line-clamp-3 text-sm leading-relaxed break-words whitespace-pre-wrap">
							<span>
								{#if item.text}
									{#each messageSegments(item.text, item.links ?? []) as seg, index (index)}
										{#if seg.href}
											<a
												href={seg.href}
												class="text-primary underline underline-offset-2"
												onclick={(event) => openLink(event, seg.href ?? '')}
											>
												{seg.text}
											</a>
										{:else}
											{seg.text}
										{/if}
									{/each}
								{:else}
									（无文字）
								{/if}
							</span>
						</p>
					</li>
				{/each}
			</ul>
			{#if hasMore}
				<div class="flex justify-center py-2">
					<Button variant="ghost" size="sm" onclick={loadMore} disabled={loading}>
						{loading ? '加载中…' : '加载更早'}
					</Button>
				</div>
			{/if}
		</div>
	{/if}
</div>
