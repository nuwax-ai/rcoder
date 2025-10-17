import React from 'react';

export const GameControls: React.FC = () => {
  return (
    <div className="mt-8 p-4 bg-gray-100 rounded-lg">
      <h3 className="text-lg font-semibold mb-3 text-center">游戏操作说明</h3>
      <div className="grid grid-cols-1 md:grid-cols-2 gap-4 text-sm">
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <span className="text-lg">🖱️</span>
            <span>
              <strong>左键点击:</strong> 揭露格子
            </span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-lg">🖱️</span>
            <span>
              <strong>右键点击:</strong> 标记/取消标记地雷
            </span>
          </div>
        </div>
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <span className="text-lg">🖱️</span>
            <span>
              <strong>双击:</strong> 快速揭露周围格子（当标记数量正确时）
            </span>
          </div>
          <div className="flex items-center gap-2">
            <span className="text-lg">😊</span>
            <span>
              <strong>笑脸按钮:</strong> 开始新游戏
            </span>
          </div>
        </div>
      </div>
      <div className="mt-4 pt-4 border-t border-gray-300 text-center text-xs text-gray-600">
        <p>目标：揭露所有非地雷格子，避免踩到地雷！</p>
        <p>数字表示周围8个格子中地雷的数量。</p>
      </div>
    </div>
  );
};