@echo off
rem Serve the Tomb Raider episode from the Rillio cache over a local range
rem server, OUTSIDE Claude's MSIX container (via explorer.exe), so Michael's
rem own shell can reach it on 127.0.0.1:8766.
cd /d F:\Projects\Code\Rillio\docs\dubbing\stage1-pipeline\s1
node range-server.js "E:\Rillio\cache\Tomb.Raider.King.2026.S01E09.1080p.CR.WEB-DL.AAC2.0.H.264-AnoZu.mkv" 8766
