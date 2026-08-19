<script lang="ts">
	import { HardDriveDownload, Inbox, LogOut } from '@lucide/svelte';

	import { app } from '$lib/app-state.svelte';
	import type { DownloadUsage } from '$lib/bindings';
	import { commands } from '$lib/bindings';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import * as Card from '$lib/components/ui/card';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import { Switch } from '$lib/components/ui/switch';

	let usage = $state<DownloadUsage | null>(null);

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

	$effect(() => {
		void app.telegram?.downloadDir;
		void commands.getDownloadUsage().then((result) => {
			if (result.status === 'ok') usage = result.data;
		});
	});
</script>

<div class="h-full overflow-y-auto">
	<div class="mx-auto flex max-w-2xl flex-col gap-4 px-6 py-6">
		<Card.Root>
			<Card.Header>
				<Card.Title class="flex items-center gap-2">
					<Inbox class="size-4" />
					应用
				</Card.Title>
				<Card.Description>bundle 与版本来自 Rust / tauri-specta</Card.Description>
			</Card.Header>
			<Card.Content class="space-y-3 text-sm">
				<p>名称：{app.appInfo?.name ?? '…'}</p>
				<p>版本：{app.appInfo?.version ?? '…'}</p>
				<p>标识：{app.appInfo?.identifier ?? '…'}</p>
				<div class="flex items-center justify-between gap-3 pt-1">
					<div class="min-w-0">
						<Label for="autostart" class="text-sm font-normal">开机自启</Label>
						<p class="text-xs text-muted-foreground">登录系统后在托盘启动，不弹出窗口</p>
					</div>
					<Switch
						id="autostart"
						checked={!!app.telegram?.autostart}
						onCheckedChange={(value) => void app.setAutostart(value)}
						disabled={app.busy}
					/>
				</div>
			</Card.Content>
		</Card.Root>

		<Card.Root>
			<Card.Header>
				<Card.Title class="flex items-center gap-2">
					<HardDriveDownload class="size-4" />
					Telegram
				</Card.Title>
				<Card.Description>凭据来自仓库根目录 .env，会话写在 app data</Card.Description>
			</Card.Header>
			<Card.Content class="space-y-3 text-sm">
				<div class="flex items-center gap-2">
					<span>连接</span>
					<Badge variant={app.telegram?.connected ? 'default' : 'secondary'}>
						{app.telegram?.connected ? '已连接' : '未连接'}
					</Badge>
				</div>
				<div class="flex items-center gap-2">
					<span>授权</span>
					<Badge variant={app.telegram?.authorized ? 'default' : 'secondary'}>
						{app.telegram?.authorized ? '已登录' : '未登录'}
					</Badge>
				</div>
				<div class="flex items-center gap-2">
					<span>会话文件</span>
					<Badge variant={app.telegram?.sessionExists ? 'default' : 'outline'}>
						{app.telegram?.sessionExists ? '已存在' : '无'}
					</Badge>
				</div>
				{#if app.authorized}
					<div class="space-y-1 pt-1">
						<p class="text-sm">当前账号</p>
						{#if app.telegram?.account}
							<p>{app.telegram.account.name}</p>
							<p class="text-xs text-muted-foreground">
								{app.telegram.account.username ? `@${app.telegram.account.username}` : '无用户名'}
								{#if app.telegram.account.phone}
									· {app.telegram.account.phone}
								{/if}
							</p>
						{:else}
							<p class="text-xs text-muted-foreground">已登录，身份尚未拉到</p>
						{/if}
					</div>
					<div class="flex items-center justify-between gap-3 pt-1">
						<div class="min-w-0">
							<p class="text-sm">退出登录</p>
							<p class="text-xs text-muted-foreground">
								作废 Telegram 会话，本地消息和已下载文件保留
							</p>
						</div>
						<Button
							variant="outline"
							size="sm"
							class="shrink-0"
							onclick={() => {
								if (confirm('退出登录？需要重新验证码。本地消息和已下载文件会保留。')) {
									void app.logout();
								}
							}}
							disabled={app.busy}
						>
							退出登录
						</Button>
					</div>
				{/if}
				<div class="flex items-start justify-between gap-3">
					<p class="min-w-0 break-all text-muted-foreground">
						下载目录：{app.telegram?.downloadDir ?? '…'}
					</p>
					<div class="flex shrink-0 gap-2">
						<Button
							variant="outline"
							size="sm"
							onclick={() => void app.openDownloadDir()}
							disabled={app.busy}
						>
							打开
						</Button>
						<Button
							variant="outline"
							size="sm"
							onclick={() => void app.changeDownloadDir()}
							disabled={app.busy}
						>
							更改…
						</Button>
					</div>
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
							<p>
								{usage.chats
									.slice(0, 4)
									.map((chat) => `${usageChatLabel(chat)} ${formatSize(chat.bytes)}`)
									.join(' · ')}
								{#if usage.chats.length > 4}
									· 共 {usage.chats.length} 个会话
								{/if}
							</p>
						{/if}
					</div>
				{/if}
				<div class="flex items-center justify-between gap-3">
					<Label for="backfill-days" class="text-xs font-normal text-muted-foreground">
						全局回爬天数（0 = 只收新，&lt;0 = 全量）
					</Label>
					<Input
						id="backfill-days"
						type="number"
						step="1"
						class="h-8 w-20"
						value={app.telegram?.backfillDays ?? 0}
						disabled={app.busy}
						onchange={(event) => void app.setGlobalBackfill(event.currentTarget.value)}
					/>
				</div>
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="download-concurrency" class="text-xs font-normal text-muted-foreground">
							回爬并行下载数（1–8）
						</Label>
						<p class="text-[11px] text-muted-foreground">只影响历史回爬，实时新消息仍串行</p>
					</div>
					<Input
						id="download-concurrency"
						type="number"
						min="1"
						max="8"
						step="1"
						class="h-8 w-20"
						value={app.telegram?.downloadConcurrency ?? 2}
						disabled={app.busy}
						onchange={(event) => void app.setDownloadConcurrency(event.currentTarget.value)}
					/>
				</div>
				<div class="flex items-center justify-between gap-3">
					<div class="min-w-0">
						<Label for="show-media" class="text-sm font-normal">消息列表显示媒体</Label>
						<p class="text-xs text-muted-foreground">仅展示已下载的图片 / 视频 / 音频</p>
					</div>
					<Switch
						id="show-media"
						checked={app.showMedia}
						onCheckedChange={(value) => void app.setShowMedia(value)}
						disabled={app.busy}
					/>
				</div>
			</Card.Content>
		</Card.Root>

		{#if app.loginStep !== 'authorized'}
			<Card.Root>
				<Card.Header>
					<Card.Title>登录</Card.Title>
					<Card.Description>手机号用国际格式。验证码会发到 Telegram 或短信。</Card.Description>
				</Card.Header>
				<Card.Content class="space-y-4">
					<div class="space-y-2">
						<Label for="phone">手机号</Label>
						<div class="flex gap-2">
							<Input
								id="phone"
								bind:value={app.phone}
								placeholder="+86 138 0000 0000"
								autocomplete="tel"
								disabled={app.busy || app.loginStep === 'needPassword'}
							/>
							<Button
								onclick={() => void app.sendCode()}
								disabled={app.busy || !app.phone.trim() || app.loginStep === 'needPassword'}
							>
								{app.busy && app.loginStep === 'idle' ? '发送中…' : '发送验证码'}
							</Button>
						</div>
					</div>

					{#if app.loginStep === 'needCode' || app.loginStep === 'needPassword'}
						<div class="space-y-2">
							<Label for="code">验证码</Label>
							<div class="flex gap-2">
								<Input
									id="code"
									bind:value={app.code}
									placeholder="12345"
									inputmode="numeric"
									autocomplete="one-time-code"
									disabled={app.busy || app.loginStep === 'needPassword'}
								/>
								<Button
									onclick={() => void app.confirmCode()}
									disabled={app.busy || !app.code.trim() || app.loginStep === 'needPassword'}
								>
									{app.busy && app.loginStep === 'needCode' ? '确认中…' : '确认'}
								</Button>
							</div>
						</div>
					{/if}

					{#if app.loginStep === 'needPassword'}
						<div class="space-y-2">
							<Label for="password">两步验证密码</Label>
							{#if app.telegram?.passwordHint}
								<p class="text-xs text-muted-foreground">提示：{app.telegram.passwordHint}</p>
							{/if}
							<div class="flex gap-2">
								<Input
									id="password"
									type="password"
									bind:value={app.password}
									placeholder="账号两步验证密码"
									autocomplete="current-password"
									disabled={app.busy}
									onkeydown={(event) => app.onPasswordKeydown(event)}
								/>
								<Button
									onclick={() => void app.confirmPassword()}
									disabled={app.busy || !app.password.trim()}
								>
									{app.busy ? '登录中…' : '登录'}
								</Button>
							</div>
						</div>
					{/if}
				</Card.Content>
			</Card.Root>
		{/if}

		<Card.Root>
			<Card.Header>
				<Card.Title class="flex items-center gap-2">
					<LogOut class="size-4" />
					退出
				</Card.Title>
				<Card.Description>关窗口会隐藏到托盘，不会退出。退出只走这里或托盘菜单。</Card.Description>
			</Card.Header>
			<Card.Content>
				<Button variant="destructive" onclick={() => commands.quitApp()}>退出应用</Button>
			</Card.Content>
		</Card.Root>
	</div>
</div>
