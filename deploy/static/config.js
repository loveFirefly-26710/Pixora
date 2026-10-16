// Pixora 运行时配置。
//
// 前后端同源（自托管常态）时留一个空对象即可。把前端放在 Vercel、Cloudflare Pages、
// EdgeOne Makers 这类静态托管上、后端在别处时，把下面这行改成后端地址：
//
//   window.__PIXORA__ = { api_base: "https://img-api.example.com" };
//
// 改完刷新页面就生效——这是运行时读的配置，不是构建期写死的。构建时也可以用
// 环境变量 PIXORA_API_BASE 让 `npm run build` 直接生成同样的一行。
window.__PIXORA__ = {};
