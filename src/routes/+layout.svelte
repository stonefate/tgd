<script lang="ts">
	import { page } from '$app/state';
	import { ChevronDown, HardDriveDownload, MessageSquare, Search, Settings } from '@lucide/svelte';
	import { onMount } from 'svelte';

	import { app } from '$lib/app-state.svelte';
	import { commands } from '$lib/bindings';
	import { buttonVariants } from '$lib/components/ui/button';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import { cn } from '$lib/utils';
	import favicon from '$lib/assets/favicon.svg';
	import './layout.css';

	let { children } = $props();

	const title = $derived(
		page.url.pathname.startsWith('/settings')
			? '设置'
			: page.url.pathname.startsWith('/downloads')
				? '下载'
				: page.url.pathname.startsWith('/search')
					? '搜索'
					: '会话'
	);

	onMount(() => {
		void app.init();
		return () => app.destroy();
	});
</script>

<svelte:head>
	<title>tgd</title>
	<link rel="icon" href={favicon} />
</svelte:head>

<div class="flex h-svh overflow-hidden bg-background text-foreground antialiased">
	<aside class="flex w-44 shrink-0 flex-col border-r bg-sidebar text-sidebar-foreground">
		<div class="px-4 py-4">
			<p class="text-sm font-semibold tracking-tight">tgd</p>
			<p class="text-[11px] tracking-wide text-muted-foreground uppercase">Desktop Guard</p>
		</div>
		<nav class="flex flex-1 flex-col gap-1 px-2">
			<a
				href="/"
				class={cn(
					'flex items-center gap-2 rounded-md px-2.5 py-2 text-sm',
					title === '会话'
						? 'bg-sidebar-accent text-sidebar-accent-foreground'
						: 'text-muted-foreground hover:bg-sidebar-accent/70 hover:text-sidebar-accent-foreground'
				)}
			>
				<MessageSquare class="size-4" />
				会话
			</a>
			<a
				href="/search"
				class={cn(
					'flex items-center gap-2 rounded-md px-2.5 py-2 text-sm',
					title === '搜索'
						? 'bg-sidebar-accent text-sidebar-accent-foreground'
						: 'text-muted-foreground hover:bg-sidebar-accent/70 hover:text-sidebar-accent-foreground'
				)}
			>
				<Search class="size-4" />
				搜索
			</a>
			<a
				href="/downloads"
				class={cn(
					'flex items-center gap-2 rounded-md px-2.5 py-2 text-sm',
					title === '下载'
						? 'bg-sidebar-accent text-sidebar-accent-foreground'
						: 'text-muted-foreground hover:bg-sidebar-accent/70 hover:text-sidebar-accent-foreground'
				)}
			>
				<HardDriveDownload class="size-4" />
				下载
			</a>
			<a
				href="/settings"
				class={cn(
					'flex items-center gap-2 rounded-md px-2.5 py-2 text-sm',
					title === '设置'
						? 'bg-sidebar-accent text-sidebar-accent-foreground'
						: 'text-muted-foreground hover:bg-sidebar-accent/70 hover:text-sidebar-accent-foreground'
				)}
			>
				<Settings class="size-4" />
				设置
			</a>
		</nav>
		<div class="flex items-center gap-2 px-4 py-3 text-xs text-muted-foreground">
			<span
				class={cn(
					'size-2 rounded-full',
					app.authorized ? 'bg-emerald-500' : 'bg-muted-foreground/40'
				)}
			></span>
			{app.authorized ? (app.telegram?.account?.name ?? '已登录') : '未登录'}
		</div>
	</aside>

	<div class="flex min-w-0 flex-1 flex-col">
		<header class="flex h-12 shrink-0 items-center gap-3 border-b px-4">
			<h1 class="text-sm font-medium">{title}</h1>
			{#if app.syncLabel && app.download}
				<p class="min-w-0 flex-1 truncate text-xs text-muted-foreground">
					{app.syncLabel} · 已处理 {app.download.processed} · 已下载 {app.download.downloaded} · 已跳过
					{app.download.skipped}
				</p>
			{:else}
				<div class="flex-1"></div>
			{/if}
			<DropdownMenu.Root>
				<DropdownMenu.Trigger
					class={buttonVariants({ variant: 'ghost', size: 'icon-sm' })}
					aria-label="窗口菜单"
				>
					<ChevronDown class="size-4" />
				</DropdownMenu.Trigger>
				<DropdownMenu.Content align="end" class="w-40 min-w-40">
					<DropdownMenu.Item onSelect={() => void commands.hideMainWindow()}>
						隐藏到托盘
					</DropdownMenu.Item>
				</DropdownMenu.Content>
			</DropdownMenu.Root>
		</header>

		{#if app.error}
			<p class="shrink-0 border-b px-4 py-2 text-sm text-destructive">{app.error}</p>
		{/if}

		<div class="min-h-0 flex-1 overflow-hidden">
			{@render children()}
		</div>
	</div>
</div>
