# STATUS: llm-generated, unreviewed — pending native-speaker QA

# 部署安装向导 (/setup)。

setup-step-1-of-2 = 第 1 步，共 2 步
setup-provider-heading = 连接你的身份提供方
setup-provider-intro = 本网关没有自己的账户——用户通过你的 OIDC 提供方登录。请在下面填写，我们会在保存任何内容之前先执行一次真实登录。

setup-field-public-url = 本网关的公开 URL
setup-field-public-url-help = 用户将要访问的地址。必须完全一致（包括 https），因为登录回调地址由它生成。

setup-redirect-uri-heading = 请在提供方处放行此回调 URI
setup-redirect-uri-help = 继续之前，把它加入该客户端的允许回调 URI 列表。提供方若不认识它，会拒绝登录。

setup-field-issuer = Issuer URL
setup-field-issuer-help = 请与提供方报告的完全一致地复制——结尾的斜杠很重要。Keycloak 省略它，Authentik 需要它。

setup-field-client-id = 客户端 ID
setup-field-client-secret = 客户端密钥

setup-field-scopes = Scopes
setup-field-scopes-help = 以空格分隔。openid 始终会请求。请保留携带组成员信息的那一个。

setup-field-roles-claim = 组 claim
setup-field-roles-claim-help = 哪个 claim 列出用户所属的组。不确定？先留着，在下一屏从你自己的令牌里挑选。

setup-test-button = 登录以进行测试
setup-test-button-help = 目前尚未保存任何内容。登录后你会回到这里。

setup-step-2-of-2 = 第 2 步，共 2 步
setup-admin-heading = 选择由谁管理本网关
setup-login-worked = 登录成功。你的提供方将你识别为：
setup-admin-intro = 下面是你的提供方实际声明的关于你的信息。请选择应授予完整管理权限的组——其他登录的人将获得普通账户。
setup-no-claims = 你的提供方没有发送任何类似组的 claim。请在下面手工填写 claim 与取值，或给该客户端添加 groups scope 后重试。

setup-or-manual = 或手动输入
setup-manual-claim = Claim
setup-manual-value = 取值
setup-manual-help = 如果应当成为管理员的组你自己并不在其中，请使用此项。这里填写的取值优先于上面的选择。

setup-finish-button = 完成安装
setup-back-button = 返回提供方设置
setup-show-token = 显示提供方发送的全部内容
