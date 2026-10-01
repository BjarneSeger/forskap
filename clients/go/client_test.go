package orgthehosterforskapd

import "testing"

func TestDefaultSocket(t *testing.T) {
	env := func(vars map[string]string) func(string) string {
		return func(key string) string { return vars[key] }
	}
	xdg := map[string]string{"XDG_RUNTIME_DIR": "/run/user/1000", "XDG_DATA_HOME": "/data"}
	for _, c := range []struct {
		name, goos, home string
		vars             map[string]string
		want             string
	}{
		{"runtime dir", "linux", "/home/me", xdg, "/run/user/1000/forskapd.socket"},
		{
			"data home", "linux", "/home/me",
			map[string]string{"XDG_DATA_HOME": "/data"},
			"/data/forskapd/forskapd.socket",
		},
		{"home", "linux", "/home/me", nil, "/home/me/.local/share/forskapd/forskapd.socket"},
		{
			"relative variables are ignored", "linux", "/home/me",
			map[string]string{"XDG_RUNTIME_DIR": "run", "XDG_DATA_HOME": "data"},
			"/home/me/.local/share/forskapd/forskapd.socket",
		},
		{
			"macOS reads no XDG variable", "darwin", "/Users/me", xdg,
			"/Users/me/Library/Application Support/forskapd/forskapd.socket",
		},
		{"no home", "linux", "", nil, ""},
		{"no home on macOS", "darwin", "", xdg, ""},
	} {
		if got := defaultSocket(c.goos, env(c.vars), c.home); got != c.want {
			t.Errorf("%s: got %q, want %q", c.name, got, c.want)
		}
	}
}
