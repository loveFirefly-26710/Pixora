/**
 * 前端只跟后端同源部署。
 *
 * 接口与静态资源全部走相对路径。这里只留下一个把后端下发的站内地址原样交给 `<img>` 的小工具：
 * 相对路径本来就可用，外链、`data:`、`blob:` 也原样返回。
 */
export function absUrl(value: string | null | undefined): string {
  return (value ?? "").trim();
}
