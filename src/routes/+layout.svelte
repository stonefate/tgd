<script lang="ts">
	import { base } from '$app/paths';
	import { page } from '$app/state';
	import { ChevronDown, HardDriveDownload, MessageSquare, Search, Settings } from '@lucide/svelte';
	import { onMount } from 'svelte';
	import type { Component } from 'svelte';

	import { app } from '$lib/app-state.svelte';
	import { commands } from '$lib/api';
	import { httpPath, isTauri } from '$lib/runtime';
	import { buttonVariants } from '$lib/components/ui/button';
	import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
	import { cn } from '$lib/utils';
	import favicon from '$lib/assets/favicon.svg';
	import './layout.css';

	let { children } = $props();

	const path = $derived(page.url.pathname.slice(base.length) || '/');
	const title = $derived(
		path.startsWith('/settings')
			? '设置'
			: path.startsWith('/downloads')
				? '下载'
				: path.startsWith('/search')
					? '搜索'
					: '会话'
	);

	const nav: { href: string; title: string; icon: Component }[] = [
		{ href: '/', title: '会话', icon: MessageSquare },
		{ href: '/search', title: '搜索', icon: Search },
		{ href: '/downloads', title: '下载', icon: HardDriveDownload },
		{ href: '/settings', title: '设置', icon: Settings }
	];

	let keyboardOpen = $state(false);

	onMount(() => {
		void app.init();
		const vv = window.visualViewport;
		const update = () => {
			keyboardOpen = !!vv && window.innerHeight - vv.height > 120;
		};
		vv?.addEventListener('resize', update);
		return () => {
			app.destroy();
			vv?.removeEventListener('resize', update);
		};
	});
</script>

<svelte:head>
	<title>tgd</title>
	<link rel="icon" href={favicon} />
</svelte:head>

<div
	class="flex h-dvh flex-col overflow-hidden bg-background text-foreground antialiased pt-[env(safe-area-inset-top)]"
>
	<div class="flex min-h-0 flex-1 overflow-hidden">
		<aside
			class={cn(
				'w-44 shrink-0 flex-col border-r bg-sidebar text-sidebar-foreground',
				isTauri() ? 'flex' : 'hidden md:flex'
			)}
		>
			<div class="px-4 py-4">
				<p class="text-sm font-semibold tracking-tight">tgd</p>
				<p class="text-[11px] tracking-wide text-muted-foreground uppercase">Desktop Guard</p>
			</div>
			<nav class="flex flex-1 flex-col gap-1 px-2">
				{#each nav as item (item.href)}
					{@const Icon = item.icon}
					<a
						href={httpPath(item.href)}
						class={cn(
							'flex items-center gap-2 rounded-md px-2.5 py-2 text-sm',
							title === item.title
								? 'bg-sidebar-accent text-sidebar-accent-foreground'
								: 'text-muted-foreground hover:bg-sidebar-accent/70 hover:text-sidebar-accent-foreground'
						)}
					>
						<Icon class="size-4" />
						{item.title}
					</a>
				{/each}
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
			<header class="flex h-12 shrink-0 items-center gap-2 border-b px-3 md:gap-3 md:px-4">
				{#if !isTauri()}
					<span
						class={cn(
							'size-2 shrink-0 rounded-full md:hidden',
							app.authorized ? 'bg-emerald-500' : 'bg-muted-foreground/40'
						)}
						title={app.authorized ? (app.telegram?.account?.name ?? '已登录') : '未登录'}
					></span>
				{/if}
				<h1 class="shrink-0 text-sm font-medium">{title}</h1>
				{#if app.syncLabel && app.download}
					<p class="min-w-0 flex-1 truncate text-[11px] text-muted-foreground md:text-xs">
						<span class="md:hidden">{app.syncLabel}</span>
						<span class="hidden md:inline">
							{app.syncLabel} · 已处理 {app.download.processed} · 已下载 {app.download.downloaded} ·
							已跳过 {app.download.skipped}
						</span>
					</p>
				{:else}
					<div class="flex-1"></div>
				{/if}
				{#if isTauri()}
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
				{/if}
			</header>

			{#if app.error}
				<p class="shrink-0 border-b px-3 py-2 text-sm text-destructive md:px-4">{app.error}</p>
			{/if}

			<div class="min-h-0 flex-1 overflow-hidden">
				{@render children()}
			</div>
		</div>
	</div>

	{#if !isTauri() && !keyboardOpen}
		<nav
			class="flex shrink-0 border-t bg-sidebar text-sidebar-foreground md:hidden pb-[env(safe-area-inset-bottom)]"
		>
			{#each nav as item (item.href)}
				{@const Icon = item.icon}
				<a
					href={httpPath(item.href)}
					class={cn(
						'flex min-h-12 min-w-0 flex-1 flex-col items-center justify-center gap-0.5 px-1 py-1.5 text-[11px]',
						title === item.title
							? 'text-sidebar-accent-foreground'
							: 'text-muted-foreground'
					)}
				>
					<Icon class="size-5" />
					{item.title}
				</a>
			{/each}
		</nav>
	{/if}
</div>
