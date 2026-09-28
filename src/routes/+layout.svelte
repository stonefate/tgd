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
	import appIcon from '$lib/assets/app-icon.png';
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
	<title>纸飞机下载器</title>
	<link rel="icon" href={appIcon} />
</svelte:head>

<div
	class="flex h-dvh flex-col overflow-hidden bg-background text-foreground antialiased pt-[env(safe-area-inset-top)]"
>
	<div class="flex min-h-0 flex-1 overflow-hidden">
		<aside
			class={cn(
				'w-52 shrink-0 flex-col border-r border-border/50 bg-sidebar/80 backdrop-blur-md text-sidebar-foreground px-3 py-4 select-none',
				isTauri() ? 'flex' : 'hidden md:flex'
			)}
		>
			<div class="px-2 pb-4 pt-1">
				<div class="flex items-center gap-2.5">
					<img src={appIcon} alt="" class="size-9 shrink-0 rounded-2xl shadow-xs" />
					<div class="min-w-0 flex-1">
						<p class="text-sm font-bold leading-tight tracking-tight text-foreground">纸飞机下载器</p>
					</div>
				</div>
			</div>
			<nav class="flex flex-1 flex-col gap-1.5 pt-1">
				{#each nav as item (item.href)}
					{@const Icon = item.icon}
					{@const active = title === item.title}
					<a
						href={httpPath(item.href)}
						class={cn(
							'group flex items-center gap-3 rounded-full px-3.5 py-2.5 text-sm font-medium transition-all duration-150 m3-press',
							active
								? 'bg-primary/15 text-primary shadow-xs'
								: 'text-muted-foreground hover:bg-muted/70 hover:text-foreground'
						)}
					>
						<Icon class={cn('size-4.5 transition-transform group-active:scale-90', active && 'text-primary')} />
						<span>{item.title}</span>
					</a>
				{/each}
			</nav>
			<div class="mt-auto pt-2">
				<div class="flex items-center gap-2.5 rounded-2xl border border-border/40 bg-card/60 px-3 py-2 text-xs shadow-2xs backdrop-blur-xs">
					<span
						class={cn(
							'size-2.5 shrink-0 rounded-full transition-colors',
							app.authorized ? 'bg-emerald-500 ring-2 ring-emerald-500/20' : 'bg-muted-foreground/40'
						)}
					></span>
					<span class="truncate font-medium text-foreground">
						{app.authorized ? (app.telegram?.account?.name ?? '已登录') : '未登录'}
					</span>
				</div>
			</div>
		</aside>

		<div class="flex min-w-0 flex-1 flex-col">
			<header class="flex h-13 shrink-0 items-center gap-2.5 border-b border-border/40 bg-background/80 px-3.5 backdrop-blur-md md:gap-3 md:px-5">
				{#if !isTauri()}
					<span
						class={cn(
							'size-2 shrink-0 rounded-full md:hidden',
							app.authorized ? 'bg-emerald-500 ring-2 ring-emerald-500/20' : 'bg-muted-foreground/40'
						)}
						title={app.authorized ? (app.telegram?.account?.name ?? '已登录') : '未登录'}
					></span>
				{/if}
				<h1 class="shrink-0 text-base font-semibold tracking-tight text-foreground">{title}</h1>
				{#if app.syncLabel && app.download}
					<div class="min-w-0 flex-1">
						<div class="inline-flex max-w-full items-center gap-1.5 rounded-full border border-border/50 bg-secondary/80 px-3 py-1 text-xs text-secondary-foreground shadow-2xs">
							<span class="size-1.5 shrink-0 animate-pulse rounded-full bg-primary"></span>
							<span class="truncate text-[11px] md:text-xs">
								<span class="md:hidden">{app.syncLabel}</span>
								<span class="hidden md:inline">
									{app.syncLabel} · 已处理 {app.download.processed} · 已下载 {app.download.downloaded} ·
									已跳过 {app.download.skipped}
								</span>
							</span>
						</div>
					</div>
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
						<DropdownMenu.Content align="end" class="w-40 min-w-40 rounded-2xl">
							<DropdownMenu.Item onSelect={() => void commands.hideMainWindow()}>
								隐藏到托盘
							</DropdownMenu.Item>
						</DropdownMenu.Content>
					</DropdownMenu.Root>
				{/if}
			</header>

			{#if app.error}
				<div class="shrink-0 border-b border-destructive/20 bg-destructive/10 px-4 py-2 text-xs font-medium text-destructive">
					{app.error}
				</div>
			{/if}

			<div class="min-h-0 flex-1 overflow-hidden bg-background">
				{@render children()}
			</div>
		</div>
	</div>

	{#if !isTauri() && !keyboardOpen}
		<nav
			class="flex shrink-0 border-t border-border/40 bg-card/90 pb-[env(safe-area-inset-bottom)] text-foreground backdrop-blur-md md:hidden select-none"
		>
			{#each nav as item (item.href)}
				{@const Icon = item.icon}
				{@const active = title === item.title}
				<a
					href={httpPath(item.href)}
					class="flex min-h-16 min-w-0 flex-1 flex-col items-center justify-center gap-1 px-1 py-1.5 transition-colors m3-press"
				>
					<div
						class={cn(
							'flex h-8 w-13 items-center justify-center rounded-full transition-all duration-200',
							active
								? 'bg-primary/18 text-primary shadow-xs'
								: 'bg-transparent text-muted-foreground'
						)}
					>
						<Icon class={cn('size-5', active && 'stroke-[2.2]')} />
					</div>
					<span
						class={cn(
							'text-[11px] leading-none transition-colors',
							active ? 'font-semibold text-primary' : 'text-muted-foreground'
						)}
					>
						{item.title}
					</span>
				</a>
			{/each}
		</nav>
	{/if}
</div>
