package tuanjie

import (
	"sync"
	"testing"
	"time"
)

// 会话池行为规范（对齐官方 CLI「一个窗口一个 session」画像）：
// 串行复用同一会话；并发全忙新建；闲置清理；超龄轮换。
func TestSessionPoolSerialReuse(t *testing.T) {
	// 独立池（不动全局，避免与其他测试互扰）
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	a := AcquireLitellmSession()
	if a == nil {
		t.Fatal("Acquire 返回 nil")
	}
	ReleaseLitellmSession(a)
	b := AcquireLitellmSession()
	ReleaseLitellmSession(b)
	if a.ID != b.ID {
		t.Fatalf("串行请求应复用同一会话：a=%s b=%s", a.ID, b.ID)
	}
}

func TestSessionPoolConcurrentNewSession(t *testing.T) {
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	a := AcquireLitellmSession() // 占住
	b := AcquireLitellmSession() // 全忙 → 新建
	if a.ID == b.ID {
		t.Fatal("并发（全忙）应新建会话而非共享")
	}
	ReleaseLitellmSession(a)
	ReleaseLitellmSession(b)
}

func TestSessionPoolIdleCleanup(t *testing.T) {
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	a := AcquireLitellmSession()
	ReleaseLitellmSession(a)
	// 手动把 lastUsed 拨回 31 分钟前 → 下次 Acquire 应清理并新建
	sessionPoolMu.Lock()
	for _, s := range sessionPool {
		s.lastUsed = s.lastUsed.Add(-sessionIdleTTL - time.Minute)
	}
	sessionPoolMu.Unlock()

	b := AcquireLitellmSession()
	ReleaseLitellmSession(b)
	if a.ID == b.ID {
		t.Fatal("闲置超 30 分钟的会话应被清理，新请求拿新会话")
	}
}

func TestSessionPoolMaxAgeRotation(t *testing.T) {
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	a := AcquireLitellmSession()
	ReleaseLitellmSession(a)
	// 年龄拨到 4h+1min（仍「最近用过」）→ 超龄轮换应生效
	sessionPoolMu.Lock()
	for _, s := range sessionPool {
		s.createdAt = s.createdAt.Add(-sessionMaxAge - time.Minute)
	}
	sessionPoolMu.Unlock()

	b := AcquireLitellmSession()
	ReleaseLitellmSession(b)
	if a.ID == b.ID {
		t.Fatal("超过 4 小时的会话应轮换，新请求拿新会话")
	}
}

func TestSessionPoolCapSharing(t *testing.T) {
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	// 占满上限（16 个并发）
	held := make([]*LitellmSession, 0, sessionPoolMaxSize)
	for i := 0; i < sessionPoolMaxSize; i++ {
		held = append(held, AcquireLitellmSession())
	}
	// 超限请求 → 共享（不再新建）
	shared := AcquireLitellmSession()
	found := false
	for _, h := range held {
		if h.ID == shared.ID {
			found = true
			break
		}
	}
	if !found {
		t.Fatal("达到池上限时应共享既有会话，而不是无限新建")
	}
	if len(sessionPool) != sessionPoolMaxSize {
		t.Fatalf("池大小应保持上限 %d，实际 %d", sessionPoolMaxSize, len(sessionPool))
	}
	for _, h := range held {
		ReleaseLitellmSession(h)
	}
	ReleaseLitellmSession(shared)
}

func TestSessionPoolConcurrentSafety(t *testing.T) {
	old := sessionPool
	sessionPool = nil
	defer func() { sessionPool = old }()

	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			s := AcquireLitellmSession()
			ReleaseLitellmSession(s)
		}()
	}
	wg.Wait()
	// 全部归还后池内会话的 inUse 应全为 0
	sessionPoolMu.Lock()
	defer sessionPoolMu.Unlock()
	for _, s := range sessionPool {
		if s.inUse != 0 {
			t.Fatalf("全部归还后 inUse 应为 0，会话 %s inUse=%d", s.ID, s.inUse)
		}
	}
}
