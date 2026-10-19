# Development

Keep changes scoped to this standalone repository. Group each coherent feature
or fix into its own commit, verify it, and push the commits to `origin` when
publishing is authorized. Do not rewrite published history or force-push.

Run from the repository directory:

```powershell
python -B -m unittest discover -s . -p test_session_bar.py -v
$env:QT_QPA_PLATFORM = 'offscreen'
python -B session_bar.py --test-ui
```

The UI checks use fixtures and do not navigate the user's desktop sessions.
For live checks, avoid submitting prompts, stopping work, or changing unrelated
chats. Use the existing account and desktop connection instead of storing keys.

Never commit `settings.json`, credentials, session databases, JSONL histories,
logs, live screenshots, caches, or temporary files. The README's curated
`preview-*.png` images may be updated from sanitized UI fixtures; inspect them
before publishing and keep personal chat data out. See [development notes](DEVELOPMENT.md).
Keep all task temporary
content under the active workspace's direct `tmp` directory. Do not create drive
letter mappings. See the workspace rules when embedding this project in a larger
checkout.
