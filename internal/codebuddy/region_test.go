// region_test.go —— 区域凭据过滤多后缀回归测试（简报 .work/current-task.md）。
//
// 起因：WorkBuddy CN 桌面端 5.7.x 把 auth.domain 从 www.codebuddy.cn 改写为
// www.workbuddy.cn，CN 只认单后缀时 region=CN 零命中、8787 起不来。
// 覆盖三件事：新域名（workbuddy.cn）判 CN 可用 / 老域名（codebuddy.cn）仍判 CN /
// INTL 只认 workbuddy.ai 且两区互不串区。
// auth 目录一律 t.TempDir() 造假经 CODEBUDDY_AUTH_DIR 隔离，池状态目录经
// CODEBUDDY_POOL_DIR 同款隔离，绝不读写真实 auth 目录。
package codebuddy

import (
	"strings"
	"testing"
	"time"
)

// containsDomain 多后缀语义：任一命中即为真；空 domain / 空后缀表 / 表内含空串
// 都不命中；子串包含语义保持（不做主机标签边界收紧，简报明令本次不做）。
func TestContainsDomainMultiSuffix(t *testing.T) {
	for _, tc := range []struct {
		name     string
		domain   string
		suffixes []string
		want     bool
	}{
		{"新域名命中第一条之后的后缀", "www.workbuddy.cn", []string{"codebuddy.cn", "workbuddy.cn"}, true},
		{"老域名命中第一条", "www.codebuddy.cn", []string{"codebuddy.cn", "workbuddy.cn"}, true},
		{"国际域名不命中 CN 表", "www.workbuddy.ai", []string{"codebuddy.cn", "workbuddy.cn"}, false},
		{"国际域名命中 INTL 表", "www.workbuddy.ai", []string{"workbuddy.ai"}, true},
		{"CN 新域名不命中 INTL 表", "www.workbuddy.cn", []string{"workbuddy.ai"}, false},
		{"空 domain 不命中", "", []string{"codebuddy.cn", "workbuddy.cn"}, false},
		{"空后缀表不命中", "www.workbuddy.cn", nil, false},
		{"表内空串不参与匹配", "www.workbuddy.cn", []string{""}, false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := containsDomain(tc.domain, tc.suffixes); got != tc.want {
				t.Fatalf("containsDomain(%q, %q) = %v, want %v", tc.domain, tc.suffixes, got, tc.want)
			}
		})
	}
}

// validateRegion：CN 认两条后缀（新/老），INTL 只认 workbuddy.ai；空 domain 与
// 异区域名如实报错，且错误文案把该区全部后缀都列出来（多值展示）。
func TestValidateRegionMultiSuffix(t *testing.T) {
	for _, tc := range []struct {
		name    string
		region  Region
		domain  string
		wantErr bool
	}{
		{"CN 认新域名", RegionCN, "www.workbuddy.cn", false},
		{"CN 认老域名", RegionCN, "www.codebuddy.cn", false},
		{"CN 不认国际域名", RegionCN, "www.workbuddy.ai", true},
		{"CN 空 domain 报错", RegionCN, "", true},
		{"INTL 认 workbuddy.ai", RegionINTL, "www.workbuddy.ai", false},
		{"INTL 不认 CN 新域名", RegionINTL, "www.workbuddy.cn", true},
		{"INTL 不认 CN 老域名", RegionINTL, "www.codebuddy.cn", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			err := validateRegion(tc.region, tc.domain)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("validateRegion(%s, %q) 应报错", tc.region, tc.domain)
				}
				if !strings.Contains(err.Error(), string(tc.region)) {
					t.Fatalf("错误文案应带区域名: %v", err)
				}
				return
			}
			if err != nil {
				t.Fatalf("validateRegion(%s, %q) 不应报错: %v", tc.region, tc.domain, err)
			}
		})
	}
	// 多值展示：CN 文案逐条列出两个后缀（老用户在报错里也能看到新域名）。
	err := validateRegion(RegionCN, "www.example.com")
	if err == nil {
		t.Fatal("异区域名应报错")
	}
	for _, s := range regionConfigs[RegionCN].DomainSuffixes {
		if !strings.Contains(err.Error(), s) {
			t.Fatalf("CN 错误文案应列出后缀 %q: %v", s, err)
		}
	}
	t.Logf("CN 报错文案: %v", err)
}

// 回归主体：Find（取文件）与建池（扫文件）两条链路上，auth.domain 决定区域归属
// ——新域名入 CN、老域名仍入 CN、国际域名只入 INTL，异区一律零命中（不串区）。
func TestFindAuthAndScanAuthNewCNDomain(t *testing.T) {
	// codebuddy.cn 只在 CN 表里，workbuddy.cn 也只在 CN 表里（两者互不包含）；
	// workbuddy.ai 只在 INTL 表里——所以 "workbuddy.cn" 不会命中 "workbuddy.ai"。
	for _, tc := range []struct {
		name      string
		domain    string
		cnHit     bool
		intlHit   bool
		uid       string
		cnPickUID string
	}{
		{"新桌面端域名 workbuddy.cn", "www.workbuddy.cn", true, false, "uid-new-cn", "uid-new-cn"},
		{"老域名 codebuddy.cn", "www.codebuddy.cn", true, false, "uid-old-cn", "uid-old-cn"},
		{"国际域名 workbuddy.ai", "www.workbuddy.ai", false, true, "uid-intl", ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			t.Setenv("CODEBUDDY_AUTH_DIR", dir)
			t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())
			path := writeAuthInfo(t, dir, "a.info", tc.domain, tc.uid, "号", time.Now())

			// Find 链路（FindAuthFileForRegion）
			gotCN := FindAuthFileForRegion(RegionCN)
			gotINTL := FindAuthFileForRegion(RegionINTL)
			if tc.cnHit {
				if gotCN != path {
					t.Fatalf("CN 应命中 %s: got %q", tc.domain, gotCN)
				}
			} else if gotCN != "" {
				t.Fatalf("CN 不该命中 %s: got %q（串区）", tc.domain, gotCN)
			}
			if tc.intlHit {
				if gotINTL != path {
					t.Fatalf("INTL 应命中 %s: got %q", tc.domain, gotINTL)
				}
			} else if gotINTL != "" {
				t.Fatalf("INTL 不该命中 %s: got %q（串区）", tc.domain, gotINTL)
			}

			// 建池链路（scanAuthFiles / NewAccountPool）
			pCN := NewAccountPool(RegionCN)
			pINTL := NewAccountPool(RegionINTL)
			if tc.cnHit {
				if pCN == nil || pCN.Size() != 1 {
					t.Fatalf("CN 池应含 1 个号，got %v", pCN)
				}
				if a := pCN.Pick(); a == nil || a.UID != tc.cnPickUID {
					t.Fatalf("CN 池 Pick 应出 %s: got %v", tc.cnPickUID, a)
				}
			} else if pCN != nil {
				t.Fatalf("CN 池不该收 %s（串区）: %d 个号", tc.domain, pCN.Size())
			}
			if tc.intlHit {
				if pINTL == nil || pINTL.Size() != 1 {
					t.Fatalf("INTL 池应含 1 个号，got %v", pINTL)
				}
				if a := pINTL.Pick(); a == nil || a.UID != tc.uid {
					t.Fatalf("INTL 池 Pick 应出 %s: got %v", tc.uid, a)
				}
			} else if pINTL != nil {
				t.Fatalf("INTL 池不该收 %s（串区）: %d 个号", tc.domain, pINTL.Size())
			}
		})
	}
}

// 同目录混放三种域名的文件：CN 池只收 CN 两域名（新桌面端 + 老用户各一号），
// INTL 池只收国际号——两区各扫各的，互不串区。
func TestScanAuthMixedDomainsRegionIsolation(t *testing.T) {
	dir := t.TempDir()
	t.Setenv("CODEBUDDY_AUTH_DIR", dir)
	t.Setenv("CODEBUDDY_POOL_DIR", t.TempDir())
	base := time.Now()
	writeAuthInfo(t, dir, "new.info", "www.workbuddy.cn", "uid-new", "新", base)
	writeAuthInfo(t, dir, "old.info", "www.codebuddy.cn", "uid-old", "老", base.Add(-time.Minute))
	writeAuthInfo(t, dir, "intl.info", "www.workbuddy.ai", "uid-intl", "际", base.Add(-2*time.Minute))

	pCN := NewAccountPool(RegionCN)
	if pCN == nil || pCN.Size() != 2 {
		t.Fatalf("CN 池应收 2 个号（新+老域名各一），got %v", pCN)
	}
	cnUIDs := map[string]bool{}
	for _, v := range pCN.Snapshot() {
		cnUIDs[v.UID] = true
	}
	if !cnUIDs["uid-new"] || !cnUIDs["uid-old"] {
		t.Fatalf("CN 池应含新老两个域名号: %v", pCN.Snapshot())
	}
	if cnUIDs["uid-intl"] {
		t.Fatal("CN 池不该收国际号（串区）")
	}
	pINTL := NewAccountPool(RegionINTL)
	if pINTL == nil || pINTL.Size() != 1 {
		t.Fatalf("INTL 池应收 1 个号，got %v", pINTL)
	}
	if a := pINTL.Pick(); a == nil || a.UID != "uid-intl" {
		t.Fatalf("INTL 池成员不符: got %v", a)
	}
}
