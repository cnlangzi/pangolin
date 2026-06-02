package cache

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"
)

// Cache 文件缓存
type Cache struct {
	Dir     string
	Enabled bool
}

func NewCache(dir string, enabled bool) *Cache {
	if enabled {
		os.MkdirAll(dir, 0755)
	}
	return &Cache{
		Dir:     dir,
		Enabled: enabled,
	}
}

// Key 生成缓存key (URL + QueryString)
func (c *Cache) Key(url string) string {
	hash := sha256.Sum256([]byte(url))
	return hex.EncodeToString(hash[:])
}

// Path 获取缓存文件路径
func (c *Cache) Path(url string) string {
	key := c.Key(url)
	return filepath.Join(c.Dir, key[:2], key[2:]+".cache")
}

// Get 获取缓存
func (c *Cache) Get(url string) (io.ReadCloser, error) {
	if !c.Enabled {
		return nil, nil
	}

	path := c.Path(url)
	f, err := os.Open(path)
	if err != nil {
		return nil, nil
	}

	// 检查是否过期
	// 从缓存文件读取元数据（这里简化处理，直接返回）
	stat, err := f.Stat()
	if err != nil {
		f.Close()
		return nil, nil
	}

	// 如果缓存文件超过7天，认为过期
	if time.Since(stat.ModTime()) > 7*24*time.Hour {
		f.Close()
		os.Remove(path)
		return nil, nil
	}

	log.Printf("📦 缓存命中: %s", url)
	return f, nil
}

// Put 存储缓存
func (c *Cache) Put(url string, resp *http.Response, body []byte) {
	if !c.Enabled || body == nil || len(body) == 0 {
		return
	}

	// 检查是否可缓存
	if !c.canCache(resp) {
		return
	}

	path := c.Path(url)
	dir := filepath.Dir(path)
	os.MkdirAll(dir, 0755)

	// 写入缓存: 元数据 + 响应头 + body
	f, err := os.Create(path)
	if err != nil {
		log.Printf("❌ 缓存写入失败: %s", err)
		return
	}
	defer f.Close()

	// 写入元数据
	meta := fmt.Sprintf("HTTP/1.1 %d %s\n", resp.StatusCode, resp.Status)
	for k, v := range resp.Header {
		meta += fmt.Sprintf("%s: %s\n", k, strings.Join(v, ", "))
	}
	meta += "---\n"
	
	f.WriteString(meta)
	f.Write(body)

	log.Printf("💾 缓存存储: %s (%d bytes)", url, len(body))
}

// canCache 检查响应是否可缓存
func (c *Cache) canCache(resp *http.Response) bool {
	// 不缓存带Set-Cookie的响应
	if _, ok := resp.Header["Set-Cookie"]; ok {
		return false
	}

	cc := resp.Header.Get("Cache-Control")
	if cc == "" {
		// 没有Cache-Control，看Expires
		if resp.Header.Get("Expires") != "" {
			return true
		}
		return false
	}

	// 解析Cache-Control
	cc = strings.ToLower(cc)
	if strings.Contains(cc, "no-store") || strings.Contains(cc, "no-cache") {
		return false
	}

	// 有max-age且>0，可以缓存
	if strings.Contains(cc, "max-age=") {
		return true
	}

	return false
}

// Clear 清理过期缓存
func (c *Cache) Clear() {
	if !c.Enabled {
		return
	}

	filepath.Walk(c.Dir, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return nil
		}
		if info.IsDir() {
			return nil
		}
		// 删除7天前的缓存
		if time.Since(info.ModTime()) > 7*24*time.Hour {
			os.Remove(path)
			log.Printf("🗑️ 删除过期缓存: %s", path)
		}
		return nil
	})
}
