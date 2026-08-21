<script lang="ts">
	import { Bot, ChevronLeft, Hash, Megaphone, RefreshCw, Users } from '@lucide/svelte';
	import { onMount } from 'svelte';

	import { app, normalizeTypes, typeOptions } from '$lib/app-state.svelte';
	import MessageList from '$lib/components/message-list.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import { httpPath, isTauri } from '$lib/runtime';
	import { cn, displayTitle } from '$lib/utils';

	let filter = $state('');

	function openChat(id: string) {
		app.selectChat(id);
		if (typeof window === 'undefined') return;
		if (isTauri() || !window.matchMedia('(max-width: 767px)').matches) return;
		if (history.state?.tgdChat === id) return;
		history.pushState({ tgdChat: id }, '');
	}

	function closeChat() {
		if (typeof history !== 'undefined' && history.state?.tgdChat) {
			history.back();
			return;
		}
		app.selectChat(null);
	}

	onMount(() => {
		const onPop = (event: PopStateEvent) => {
			const id = (event.state as { tgdChat?: string } | null)?.tgdChat ?? null;
			app.selectChat(id);
		};
		window.addEventListener('popstate', onPop);
		return () => window.removeEventListener('popstate', onPop);
	});

	const filtered = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		if (!q) return app.chats;
		return app.chats.filter(
			(chat) =>
				chat.title.toLowerCase().includes(q) ||
				(chat.alias?.toLowerCase().includes(q) ?? false) ||
				(chat.username?.toLowerCase().includes(q) ?? false) ||
				chat.id.includes(q)
		);
	});
</script>

<div class="flex h-full min-h-0">
	<section
		class={cn(
			'flex min-h-0 w-full shrink-0 flex-col border-r md:w-72',
			!isTauri() && app.selectedChatId && 'max-md:hidden'
		)}
	>
		<div class="flex items-center gap-2 border-b px-3 py-2">
			<Input bind:value={filter} placeholder="搜索会话" class="h-8" />
			<Button
				variant="outline"
				size="icon-sm"
				onclick={() => void app.refreshChats()}
				disabled={app.busy || !app.authorized}
				aria-label="刷新会话"
			>
				<RefreshCw class="size-4" />
			</Button>
		</div>
		<div class="text-muted-foreground flex items-center gap-1.5 px-3 py-2 text-xs">
			<Hash class="size-3" />
			{#if app.authorized}
				共 {app.chats.length} 个，已监听 {app.watchedCount} 个
			{:else}
				登录后可查询
			{/if}
		</div>
		<div class="min-h-0 flex-1 overflow-y-auto">
			{#if !app.authorized}
				<div class="text-muted-foreground space-y-3 px-4 py-8 text-sm">
					<p>先登录，再拉列表。</p>
					<Button href={httpPath('/settings')} variant="outline" size="sm">去设置登录</Button>
				</div>
			{:else if filtered.length === 0}
				<p class="text-muted-foreground px-4 py-8 text-sm">
					{filter.trim() ? '没有匹配的会话。' : '还没有群组、频道或机器人。'}
				</p>
			{:else}
				<ul>
					{#each filtered as chat (chat.id)}
						<li>
							<button
								type="button"
								class={cn(
									'hover:bg-muted/60 flex w-full items-center gap-2 px-3 py-3 text-left text-sm md:py-2',
									app.selectedChatId === chat.id && 'bg-muted',
									!chat.watched && 'opacity-60'
								)}
								onclick={() => openChat(chat.id)}
							>
								<span
									class={cn(
										'size-1.5 shrink-0 rounded-full',
										chat.watched ? 'bg-emerald-500' : 'bg-transparent'
									)}
								></span>
								<div class="min-w-0 flex-1">
									<p class="truncate font-medium">{displayTitle(chat)}</p>
									<p class="text-muted-foreground truncate text-xs">
										{#if chat.alias}
											{chat.title}
											{chat.username ? ` · @${chat.username}` : ''}
										{:else}
											{chat.username ? `@${chat.username}` : chat.id}
										{/if}
									</p>
								</div>
								<Badge variant="secondary" class="shrink-0">
									{#if chat.kind === 'group'}
										<Users class="size-3" />
										群组
									{:else if chat.kind === 'bot'}
										<Bot class="size-3" />
										机器人
									{:else}
										<Megaphone class="size-3" />
										频道
									{/if}
								</Badge>
							</button>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	</section>

	<section
		class={cn(
			'flex min-h-0 min-w-0 flex-1 flex-col',
			!isTauri() && !app.selectedChatId && 'max-md:hidden'
		)}
	>
		{#if !app.authorized}
			<div class="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-3 px-4 text-sm">
				<p>登录后选择一个会话查看消息。</p>
				<Button href={httpPath('/settings')} variant="outline" size="sm">去设置登录</Button>
			</div>
		{:else if !app.selectedChat}
			<p class="text-muted-foreground hidden flex-1 items-center justify-center text-sm md:flex">
				选一个会话
			</p>
		{:else}
			{@const chat = app.selectedChat}
			<div class="space-y-2 border-b px-3 py-3 md:px-4">
				<div class="flex items-start justify-between gap-3">
					<div class="flex min-w-0 items-start gap-1">
						<Button
							variant="ghost"
							size="icon-sm"
							class="mt-0.5 shrink-0 md:hidden"
							onclick={closeChat}
							aria-label="返回会话列表"
						>
							<ChevronLeft class="size-4" />
						</Button>
						<div class="min-w-0">
							<h2 class="truncate text-sm font-medium">{displayTitle(chat)}</h2>
							<p class="text-muted-foreground truncate text-xs">
								{#if chat.alias}
									{chat.title}
									{chat.username ? ` · @${chat.username}` : ''}
								{:else}
									{chat.username ? `@${chat.username}` : chat.id}
								{/if}
							</p>
						</div>
					</div>
					<div class="flex shrink-0 flex-wrap items-center justify-end gap-2">
						<Button
							variant="outline"
							size="sm"
							class="text-destructive h-7 px-2 text-xs md:h-6"
							disabled={app.busy}
							onclick={() => {
								const days = chat.backfillDays;
								const extra = !chat.watched
									? '当前未监听，清完后不会自动重爬。'
									: days === 0
										? '当前回爬是 0，只会收之后的新消息。'
										: `将按 ${days} 天重爬。`;
								if (
									confirm(
										`清除「${displayTitle(chat)}」的本地消息并重爬？本地还在的媒体会跳过，缺的会重下。${extra}`
									)
								) {
									void app.clearChatMessages(chat);
								}
							}}
						>
							清除消息
						</Button>
						<div class="text-muted-foreground flex items-center gap-2 text-xs">
							监听下载
							<Switch
								size="sm"
								checked={chat.watched}
								onCheckedChange={(value) => void app.setWatched(chat, value)}
								disabled={app.busy}
							/>
						</div>
					</div>
				</div>
				<div class="text-muted-foreground flex items-center gap-2 text-xs">
					别名
					<Input
						class="h-7 min-w-0 flex-1 px-1.5 text-xs md:h-6 md:w-40 md:flex-none"
						value={chat.alias ?? ''}
						placeholder={chat.title}
						onchange={(event) => void app.setChatAlias(chat, event.currentTarget.value)}
					/>
				</div>
				{#if chat.watched}
					<div class="flex flex-wrap items-center gap-2">
						{#each typeOptions as option}
							<Button
								variant={normalizeTypes(chat.types)[option.key] ? 'default' : 'outline'}
								size="sm"
								class="h-8 px-2.5 text-xs md:h-6 md:px-2"
								onclick={() =>
									void app.setType(chat, option.key, !normalizeTypes(chat.types)[option.key])}
							>
								{option.label}
							</Button>
						{/each}
						<div class="text-muted-foreground flex items-center gap-1 text-xs">
							回爬
							<Input
								type="number"
								step="1"
								class="h-6 w-16 px-1.5 text-xs"
								value={chat.backfillDaysOverride ?? ''}
								placeholder={`${chat.backfillDays}`}
								onchange={(event) => void app.setChatBackfill(chat, event.currentTarget.value)}
							/>
							天
						</div>
					</div>
				{/if}
			</div>
			{#key `${chat.id}:${app.messageEpoch}`}
				<MessageList chatId={chat.id} />
			{/key}
		{/if}
	</section>
</div>
