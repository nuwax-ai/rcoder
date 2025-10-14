/**
 * Request ID 传递机制分析工具 (JavaScript版本)
 * 用于代码级别验证 request_id 的传递流程
 */

/**
 * 模拟 RCoder 的 request_id 传递机制
 */
class RequestIdTracker {
  constructor() {
    this.SESSION_REQUEST_CONTEXT = new Map(); // project_id -> request_id
  }

  /**
   * 生成随机 request_id
   */
  generateRequestId() {
    return `req_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
  }

  /**
   * 生成随机 project_id
   */
  generateProjectId() {
    return `proj_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
  }

  /**
   * 生成随机 session_id
   */
  generateSessionId() {
    return `sess_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
  }

  /**
   * 模拟 ChatHandler 处理 request_id
   */
  simulateChatHandler(request) {
    console.log(`🔧 [ChatHandler] 处理请求: request_id=${request.request_id}`);

    // 1. 确定或生成 request_id (对应 chat_handler.rs:192-196)
    const requestId = request.request_id || this.generateRequestId();
    console.log(`✅ [ChatHandler] 确定 request_id: ${requestId}`);

    // 2. 构建 ChatPrompt (对应 chat_handler.rs:198-208)
    const chatPrompt = {
      project_id: request.project_id || this.generateProjectId(),
      session_id: request.session_id || this.generateSessionId(),
      prompt: request.prompt,
      request_id: requestId,
    };

    // 3. 返回响应 (对应 chat_handler.rs:220-225)
    const response = {
      project_id: chatPrompt.project_id,
      session_id: chatPrompt.session_id,
      error: null,
      request_id: request.request_id, // 注意：这里返回原始请求的 request_id
    };

    console.log(`📤 [ChatHandler] 返回响应: request_id=${response.request_id}`);
    return response;
  }

  /**
   * 模拟 AcpAgent 将 request_id 放入 PromptRequest.meta
   */
  simulateAcpAgent(chatPrompt) {
    console.log(`🔧 [AcpAgent] 处理 ChatPrompt: request_id=${chatPrompt.request_id}`);

    // 1. 将 request_id 放入 meta 字段 (对应 acp_agent.rs:321-328)
    const meta = chatPrompt.request_id ? {
      "request_id": chatPrompt.request_id
    } : null;

    const promptRequest = {
      meta: meta,
      session_id: chatPrompt.session_id,
      prompt: chatPrompt.prompt,
    };

    console.log(`📤 [AcpAgent] PromptRequest.meta: ${JSON.stringify(meta)}`);
    return promptRequest;
  }

  /**
   * 模拟 ChannelUtils 从 PromptRequest.meta 提取 request_id
   */
  simulateChannelUtils(promptRequest, projectId) {
    console.log(`🔧 [ChannelUtils] 处理 PromptRequest: project_id=${projectId}`);

    // 1. 从 PromptRequest.meta 中提取 request_id (对应 channel_utils.rs:90-103)
    let requestId;
    if (promptRequest.meta) {
      const reqId = promptRequest.meta["request_id"];
      requestId = typeof reqId === 'string' ? reqId : undefined;
      console.log(`🔍 [ChannelUtils] 从 meta 提取 request_id=${requestId}`);
    } else {
      console.log(`⚠️ [ChannelUtils] PromptRequest.meta 为空`);
    }

    // 2. 将 request_id 存入 SESSION_REQUEST_CONTEXT (对应 channel_utils.rs:111-123)
    if (requestId) {
      this.SESSION_REQUEST_CONTEXT.set(projectId, requestId);
      console.log(`✅ [ChannelUtils] 将 request_id=${requestId} 存入 SESSION_REQUEST_CONTEXT`);
    }

    // 3. 发送 SessionPromptStart 通知 (对应 channel_utils.rs:40-47)
    const startNotify = {
      type: 'start',
      session_id: promptRequest.session_id,
      request_id: requestId,
    };
    console.log(`📤 [ChannelUtils] 发送 SessionPromptStart: request_id=${startNotify.request_id}`);
  }

  /**
   * 模拟 SessionNotification 回调中获取 request_id
   */
  simulateSessionNotification(sessionId, projectId) {
    console.log(`🔧 [SessionNotification] 处理通知: session_id=${sessionId}`);

    // 1. 从 SESSION_REQUEST_CONTEXT 获取 request_id (对应 mod.rs:175-194)
    const requestId = this.SESSION_REQUEST_CONTEXT.get(projectId);

    if (requestId) {
      console.log(`🔍 [SessionNotification] 从 SESSION_REQUEST_CONTEXT 获取 request_id=${requestId}`);
    } else {
      console.log(`⚠️ [SessionNotification] 未找到 request_id`);
    }

    // 2. 发送通知
    const notification = {
      type: 'update',
      session_id: sessionId,
      request_id: requestId,
    };
    console.log(`📤 [SessionNotification] 发送通知: request_id=${notification.request_id}`);

    return notification;
  }

  /**
   * 执行完整的 request_id 传递流程测试
   */
  runCompleteFlow(request) {
    const steps = [];
    let success = true;
    let requestIdMatch = false;

    try {
      console.log('\n🚀 开始 Request ID 传递流程测试');
      console.log('='.repeat(50));

      // 步骤1: ChatHandler 处理
      console.log('\n📋 步骤1: ChatHandler 处理请求');
      const response = this.simulateChatHandler(request);
      steps.push(`ChatHandler: ${request.request_id} -> ${response.request_id}`);

      // 步骤2: AcpAgent 处理
      console.log('\n📋 步骤2: AcpAgent 构建 PromptRequest');
      const promptRequest = this.simulateAcpAgent({
        project_id: response.project_id,
        session_id: response.session_id,
        prompt: request.prompt,
        request_id: response.request_id,
      });
      steps.push(`AcpAgent: ${response.request_id} -> meta`);

      // 步骤3: ChannelUtils 处理
      console.log('\n📋 步骤3: ChannelUtils 提取并存储 request_id');
      this.simulateChannelUtils(promptRequest, response.project_id);
      const storedRequestId = this.SESSION_REQUEST_CONTEXT.get(response.project_id);
      steps.push(`ChannelUtils: meta -> SESSION_REQUEST_CONTEXT[${storedRequestId}]`);

      // 步骤4: SessionNotification 处理
      console.log('\n📋 步骤4: SessionNotification 回调获取 request_id');
      const notification = this.simulateSessionNotification(
        response.session_id,
        response.project_id
      );
      steps.push(`SessionNotification: SESSION_REQUEST_CONTEXT -> ${notification.request_id}`);

      // 验证 request_id 是否一致传递
      const originalRequestId = request.request_id;
      const finalRequestId = notification.request_id;

      requestIdMatch = originalRequestId === finalRequestId;

      console.log('\n📊 验证结果:');
      console.log(`   原始 request_id: ${originalRequestId}`);
      console.log(`   最终 request_id: ${finalRequestId}`);
      console.log(`   是否匹配: ${requestIdMatch ? '✅ 是' : '❌ 否'}`);

      if (requestIdMatch) {
        console.log('\n🎉 Request ID 传递机制验证成功！');
      } else {
        console.log('\n⚠️  Request ID 传递机制存在问题');
        success = false;
      }

    } catch (error) {
      console.error('\n💥 测试过程中发生错误:', error);
      steps.push(`错误: ${error.message}`);
      success = false;
    }

    return {
      success,
      requestIdMatch,
      steps
    };
  }

  // 清理测试数据
  clear() {
    this.SESSION_REQUEST_CONTEXT.clear();
  }
}

/**
 * 运行多种场景的 request_id 传递测试
 */
function runRequestIdTests() {
  console.log('🧪 RCoder Request ID 传递机制代码分析测试');
  console.log('='.repeat(60));

  const tracker = new RequestIdTracker();
  const testResults = [];

  // 测试场景1: 自定义 request_id
  console.log('\n🎯 测试场景1: 自定义 request_id');
  const test1 = tracker.runCompleteFlow({
    prompt: '测试自定义 request_id',
    project_id: 'test_project_1',
    session_id: 'test_session_1',
    request_id: 'custom_req_12345',
  });
  testResults.push({ name: '自定义 request_id', ...test1 });
  tracker.clear();

  // 测试场景2: 空 request_id (系统自动生成)
  console.log('\n🎯 测试场景2: 空 request_id (系统自动生成)');
  const test2 = tracker.runCompleteFlow({
    prompt: '测试自动生成 request_id',
    project_id: 'test_project_2',
    session_id: 'test_session_2',
    request_id: undefined,
  });
  testResults.push({ name: '自动生成 request_id', ...test2 });
  tracker.clear();

  // 测试场景3: 特殊字符 request_id
  console.log('\n🎯 测试场景3: 特殊字符 request_id');
  const test3 = tracker.runCompleteFlow({
    prompt: '测试特殊字符 request_id',
    project_id: 'test_project_3',
    session_id: 'test_session_3',
    request_id: 'special-test_123_ABC-xyz',
  });
  testResults.push({ name: '特殊字符 request_id', ...test3 });
  tracker.clear();

  // 测试场景4: 长字符串 request_id
  console.log('\n🎯 测试场景4: 长字符串 request_id');
  const test4 = tracker.runCompleteFlow({
    prompt: '测试长字符串 request_id',
    project_id: 'test_project_4',
    session_id: 'test_session_4',
    request_id: 'very_long_request_id_' + 'a'.repeat(100),
  });
  testResults.push({ name: '长字符串 request_id', ...test4 });
  tracker.clear();

  // 生成测试报告
  console.log('\n📊 测试报告');
  console.log('='.repeat(50));

  const totalTests = testResults.length;
  const successfulTests = testResults.filter(r => r.success).length;
  const matchedTests = testResults.filter(r => r.requestIdMatch).length;

  console.log(`总测试数: ${totalTests}`);
  console.log(`成功测试: ${successfulTests}`);
  console.log(`Request ID 匹配: ${matchedTests}`);
  console.log(`成功率: ${((successfulTests / totalTests) * 100).toFixed(1)}%`);
  console.log(`匹配率: ${((matchedTests / totalTests) * 100).toFixed(1)}%`);

  console.log('\n📋 详细结果:');
  testResults.forEach((result, index) => {
    console.log(`${index + 1}. ${result.name}`);
    console.log(`   ✅ 成功: ${result.success ? '是' : '否'}`);
    console.log(`   🎯 匹配: ${result.requestIdMatch ? '是' : '否'}`);
    console.log(`   📝 步骤: ${result.steps.length} 个`);
  });

  if (matchedTests === totalTests) {
    console.log('\n🎉 所有测试通过！Request ID 传递机制代码分析验证成功！');
    return true;
  } else {
    console.log('\n⚠️  部分测试失败，Request ID 传递机制可能存在问题');
    return false;
  }
}

// 如果直接运行此文件，执行测试
if (require.main === module) {
  runRequestIdTests();
}

module.exports = { RequestIdTracker, runRequestIdTests };