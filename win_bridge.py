import http.server
import subprocess
import json
import os
import sys

# 配置
PORT = 8080
# 默认在脚本所在目录执行
BASE_DIR = os.path.dirname(os.path.abspath(__file__))

class CommandHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        content_length = int(self.headers['Content-Length'])
        post_data = self.rfile.read(content_length)
        
        try:
            data = json.loads(post_data.decode('utf-8'))
            command = data.get('command')
            cwd = data.get('cwd', BASE_DIR)

            print(f"--- Running: {command} ---")
            
            # 执行命令并捕获输出
            # 使用 shell=True 以支持 cargo, dir 等内置命令
            process = subprocess.run(
                command,
                shell=True,
                capture_output=True,
                text=True,
                cwd=cwd,
                encoding='utf-8',
                errors='replace'
            )
            
            response = {
                'stdout': process.stdout,
                'stderr': process.stderr,
                'exit_code': process.returncode
            }
            print(f"Done. Exit code: {process.returncode}")
            
        except Exception as e:
            print(f"Error: {str(e)}")
            response = {'error': str(e)}

        self.send_response(200)
        self.send_header('Content-type', 'application/json')
        self.end_headers()
        self.wfile.write(json.dumps(response).encode('utf-8'))

    def log_message(self, format, *args):
        # 禁用默认的访问日志以保持界面整洁
        return

if __name__ == '__main__':
    # 监听 0.0.0.0 以允许从 WSL 访问（即使是旧版 WSL2）
    server = http.server.HTTPServer(('0.0.0.0', PORT), CommandHandler)
    print(f"==========================================")
    print(f" Windows Command Bridge 正在运行")
    print(f" 监听端口: {PORT}")
    print(f" 项目目录: {BASE_DIR}")
    print(f"==========================================")
    print(f"请在 Windows 端保持此窗口开启。")
    
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n正在关闭 Bridge...")
        sys.exit(0)
