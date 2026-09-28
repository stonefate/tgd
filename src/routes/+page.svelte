<script lang="ts">
	import { ChevronLeft, Hash, Megaphone, MessageSquare, RefreshCw, Search, Users } from '@lucide/svelte';
	import { onMount } from 'svelte';

	import { app, hasMediaTypes, normalizeTypes, typeOptions } from '$lib/app-state.svelte';
	import MessageList from '$lib/components/message-list.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Switch } from '$lib/components/ui/switch';
	import { httpPath, isTauri } from '$lib/runtime';
	import { cn, displayTitle } from '$lib/utils';

	type WatchFilter = 'all' | 'watched' | 'unwatched';

	const WATCH_FILTERS: { id: WatchFilter; label: string }[] = [
		{ id: 'all', label: '全部' },
		{ id: 'watched', label: '已监听' },
		{ id: 'unwatched', label: '未监听' }
	];

	let filter = $state('');
	let watchFilter = $state<WatchFilter>('all');

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
		return app.chats.filter((chat) => {
			if (watchFilter === 'watched' && !chat.watched) return false;
			if (watchFilter === 'unwatched' && chat.watched) return false;
			if (!q) return true;
			return (
				chat.title.toLowerCase().includes(q) ||
				(chat.alias?.toLowerCase().includes(q) ?? false) ||
				(chat.username?.toLowerCase().includes(q) ?? false) ||
				chat.id.includes(q)
			);
		});
	});
</script>

<div class="flex h-full min-h-0 bg-background md:p-3 md:gap-3">
	<section
		class={cn(
			'flex min-h-0 w-full shrink-0 flex-col bg-card md:w-80 md:rounded-3xl md:border md:border-border/50 md:shadow-xs overflow-hidden',
			!isTauri() && app.selectedChatId && 'max-md:hidden'
		)}
	>
		<div class="flex items-center gap-2 p-3 pb-2">
			<div class="relative min-w-0 flex-1">
				<Search class="pointer-events-none absolute left-3 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
				<Input
					bind:value={filter}
					placeholder="搜索会话…"
					class="h-9 rounded-full border-0 bg-muted/60 pl-8.5 pr-3 text-xs focus-visible:ring-2 focus-visible:ring-primary/40"
				/>
			</div>
			<Button
				variant="ghost"
				size="icon-sm"
				class="size-9 shrink-0 rounded-full hover:bg-muted/70"
				onclick={() => void app.refreshChats()}
				disabled={app.busy || !app.authorized}
				aria-label="刷新会话"
			>
				<RefreshCw class="size-4" />
			</Button>
		</div>
		<div class="space-y-2 px-3 pb-2.5 pt-0.5">
			<div class="flex items-center gap-1.5 overflow-x-auto no-scrollbar">
				{#each WATCH_FILTERS as option (option.id)}
					{@const active = watchFilter === option.id}
					<button
						type="button"
						class={cn(
							'rounded-full px-3 py-1 text-xs font-medium transition-all m3-press',
							active
								? 'bg-primary text-primary-foreground shadow-2xs'
								: 'bg-muted/60 text-muted-foreground hover:bg-muted hover:text-foreground'
						)}
						onclick={() => (watchFilter = option.id)}
					>
						{option.label}
					</button>
				{/each}
			</div>
			<div class="flex items-center gap-1.5 px-0.5 text-[11px] text-muted-foreground">
				<Hash class="size-3 text-primary/70" />
				{#if app.authorized}
					共 {app.chats.length} 个，已监听 {app.watchedCount} 个
				{:else}
					登录后可查询
				{/if}
			</div>
		</div>
		<div class="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
			{#if !app.authorized}
				<div class="space-y-3 px-4 py-8 text-center text-sm text-muted-foreground">
					<p>先登录，再拉列表。</p>
					<Button href={httpPath('/settings')} variant="outline" size="sm" class="rounded-full">去设置登录</Button>
				</div>
			{:else if filtered.length === 0}
				<p class="px-4 py-8 text-center text-sm text-muted-foreground">
					{#if filter.trim()}
						没有匹配的会话。
					{:else if watchFilter === 'watched'}
						还没有已监听的会话。
					{:else if watchFilter === 'unwatched'}
						没有未监听的会话。
					{:else}
						还没有群组或频道。
					{/if}
				</p>
			{:else}
				<ul class="space-y-1">
					{#each filtered as chat (chat.id)}
						{@const isSelected = app.selectedChatId === chat.id}
						<li>
							<button
								type="button"
								class={cn(
									'group flex w-full items-center gap-2.5 rounded-2xl px-3 py-2.5 text-left text-sm transition-all duration-150 m3-press',
									isSelected
										? 'bg-primary/12 text-foreground font-medium shadow-2xs'
										: 'text-foreground/90 hover:bg-muted/60',
									!chat.watched && 'opacity-65'
								)}
								onclick={() => openChat(chat.id)}
							>
								<span
									class={cn(
										'size-2 shrink-0 rounded-full transition-all',
										chat.watched ? 'bg-emerald-500 ring-2 ring-emerald-500/20' : 'bg-muted-foreground/30'
									)}
								></span>
								<div class="min-w-0 flex-1">
									<p class="truncate font-medium leading-snug">{displayTitle(chat)}</p>
									<p class="truncate text-xs text-muted-foreground">
										{#if chat.commentOfTitle}
											{chat.commentOfTitle} 的评论
										{:else if chat.alias}
											{chat.title}
											{chat.username ? ` · @${chat.username}` : ''}
										{:else}
											{chat.username ? `@${chat.username}` : chat.id}
										{/if}
									</p>
								</div>
								<Badge
									variant={isSelected ? 'default' : 'secondary'}
									class="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-normal"
								>
									{#if chat.guest}
										未加入
									{:else if chat.commentOfId}
										<MessageSquare class="size-3" />
										评论
									{:else if chat.kind === 'group'}
										<Users class="size-3" />
										群组
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
			'flex min-h-0 min-w-0 flex-1 flex-col bg-card md:rounded-3xl md:border md:border-border/50 md:shadow-xs overflow-hidden',
			!isTauri() && !app.selectedChatId && 'max-md:hidden'
		)}
	>
		{#if !app.authorized}
			<div class="flex flex-1 flex-col items-center justify-center gap-3 px-4 text-sm text-muted-foreground">
				<p>登录后选择一个会话查看消息。</p>
				<Button href={httpPath('/settings')} variant="outline" size="sm" class="rounded-full">去设置登录</Button>
			</div>
		{:else if !app.selectedChat}
			<div class="hidden flex-1 flex-col items-center justify-center gap-2 text-sm text-muted-foreground md:flex">
				<div class="flex size-14 items-center justify-center rounded-3xl bg-muted/40 text-muted-foreground/60">
					<MessageSquare class="size-7" />
				</div>
				<p>选择左侧会话以查看消息与文件</p>
			</div>
		{:else}
			{@const chat = app.selectedChat}
			<div class="space-y-2.5 border-b border-border/40 bg-card/70 px-3.5 py-3 backdrop-blur-md md:px-5">
				<div class="flex items-start justify-between gap-3">
					<div class="flex min-w-0 items-start gap-1.5">
						<Button
							variant="ghost"
							size="icon-sm"
							class="mt-0.5 size-8 shrink-0 rounded-full bg-muted/50 hover:bg-muted md:hidden"
							onclick={closeChat}
							aria-label="返回会话列表"
						>
							<ChevronLeft class="size-4" />
						</Button>
						<div class="min-w-0">
							<h2 class="truncate text-sm font-semibold tracking-tight text-foreground">{displayTitle(chat)}</h2>
							<p class="truncate text-xs text-muted-foreground">
								{#if chat.guest}
									未加入公开预览{chat.username ? ` · @${chat.username}` : ''}
								{:else if chat.commentOfTitle}
									{chat.commentOfTitle} 的评论
								{:else if chat.alias}
									{chat.title}
									{chat.username ? ` · @${chat.username}` : ''}
								{:else}
									{chat.username ? `@${chat.username}` : chat.id}
								{/if}
							</p>
							{#if chat.kind === 'channel' && chat.discussionId}
								<p class="mt-0.5 text-xs text-muted-foreground">
									<button
										type="button"
										class="text-primary font-medium underline underline-offset-2 hover:opacity-80"
										onclick={() => openChat(chat.discussionId ?? '')}
									>
										打开评论区
									</button>
								</p>
							{/if}
						</div>
					</div>
					<div class="flex shrink-0 flex-wrap items-center justify-end gap-1.5">
						<Button
							variant="outline"
							size="sm"
							class="h-7 rounded-full px-2.5 text-xs"
							disabled={
								app.busy ||
								!app.authorized ||
								!chat.watched ||
								chat.backfillDays === 0 ||
								!hasMediaTypes(chat.types)
							}
							title={
								!chat.watched
									? '未监听'
									: chat.backfillDays === 0
										? '回爬天数是 0，只收新消息'
										: !hasMediaTypes(chat.types)
											? '没有勾选媒体类型'
											: '按当前类型和天数检查本地文件，缺失则重下'
							}
							onclick={() => void app.checkChatMedia(chat)}
						>
							检查文件
						</Button>
						<Button
							variant="outline"
							size="sm"
							class="h-7 rounded-full px-2.5 text-xs text-destructive hover:bg-destructive/10"
							disabled={app.busy}
							onclick={() => {
								const days = chat.backfillDays;
								const extra = !chat.watched
									? '当前未监听，清完后不会自动重爬。'
									: days === 0
										? '当前回爬是 0，只会收之后的新消息。'
										: `将按 ${days} 天从头回爬。`;
								const comments =
									chat.kind === 'channel' && chat.discussionId
										? '频道会连同评论区一起清除。'
										: '';
								if (
									confirm(
										`清除「${displayTitle(chat)}」的本地消息和已下载媒体（含未下完的 .part），从 0 重爬？不可撤销。${comments}${extra}`
									)
								) {
									void app.clearChatMessages(chat);
								}
							}}
						>
							清除并重爬
						</Button>
						<div class="flex items-center gap-1.5 rounded-full border border-border/50 bg-muted/40 px-2.5 py-1 text-xs text-muted-foreground">
							<span>监听下载</span>
							<Switch
								size="sm"
								checked={chat.watched}
								onCheckedChange={(value) => void app.setWatched(chat, value)}
								disabled={app.busy}
							/>
						</div>
					</div>
				</div>
				<div class="flex items-center gap-2 text-xs text-muted-foreground">
					<span class="shrink-0 font-medium">别名</span>
					<Input
						class="h-7 min-w-0 flex-1 rounded-full border-0 bg-muted/60 px-3 text-xs md:w-44 md:flex-none"
						value={chat.alias ?? ''}
						placeholder={chat.title}
						onchange={(event) => void app.setChatAlias(chat, event.currentTarget.value)}
					/>
				</div>
				{#if chat.watched}
					<div class="flex flex-wrap items-center gap-1.5 pt-0.5">
						{#each typeOptions as option}
							{@const selected = normalizeTypes(chat.types)[option.key]}
							<button
								type="button"
								class={cn(
									'rounded-full px-2.5 py-1 text-xs font-medium transition-all m3-press',
									selected
										? 'bg-primary text-primary-foreground shadow-2xs'
										: 'bg-muted/60 text-muted-foreground hover:bg-muted hover:text-foreground'
								)}
								onclick={() =>
									void app.setType(chat, option.key, !selected)}
							>
								{option.label}
							</button>
						{/each}
						<div class="flex items-center gap-1 rounded-full border border-border/50 bg-muted/30 px-2 py-0.5 text-xs text-muted-foreground">
							<span>回爬</span>
							<Input
								type="number"
								step="1"
								class="h-5 w-12 rounded-full border-0 bg-muted/80 px-1 text-center text-xs"
								value={chat.backfillDaysOverride ?? ''}
								placeholder={`${chat.backfillDays}`}
								onchange={(event) => void app.setChatBackfill(chat, event.currentTarget.value)}
							/>
							<span>天</span>
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
